use crate::util::{read, read_u64};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Sensor {
    /// Stable chip id, as node_exporter names it.
    pub chip: String,
    /// Driver name from the `name` file, e.g. k10temp.
    pub chip_name: String,
    /// Sensor file prefix, e.g. temp1.
    pub sensor: String,
    pub label: Option<String>,
    pub kind: SensorKind,
    pub value: f64,
    pub max: Option<f64>,
    pub crit: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SensorKind {
    Temp,
    Fan,
}

/// Mirrors node_exporter's cleanMetricName: lowercase, [^a-z0-9:_] to _, trimmed of _.
fn clean(s: &str) -> String {
    let out: String = s
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == ':' || c == '_' { c } else { '_' })
        .collect();
    out.trim_matches('_').to_string()
}

/// Same precedence as node_exporter's hwmonName: <devtype>_<devname>, then devname, then the name file.
pub fn chip_id(dir: &Path) -> Option<String> {
    if let Ok(dev) = std::fs::canonicalize(dir.join("device")) {
        let dev_name = dev.file_name().map(|s| clean(&s.to_string_lossy())).unwrap_or_default();
        let dev_type = dev.parent().and_then(|p| p.file_name()).map(|s| clean(&s.to_string_lossy())).unwrap_or_default();
        if !dev_type.is_empty() && !dev_name.is_empty() {
            return Some(format!("{dev_type}_{dev_name}"));
        }
        if !dev_name.is_empty() {
            return Some(dev_name);
        }
    }
    let name = clean(&read(dir.join("name"))?);
    (!name.is_empty()).then_some(name)
}

pub fn collect(sys: &Path) -> Vec<Sensor> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(sys.join("class/hwmon")) else { return out };
    let mut dirs: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    dirs.sort();
    for dir in dirs {
        let Some(chip) = chip_id(&dir) else { continue };
        let chip_name = read(dir.join("name")).map(|s| s.trim().to_string()).unwrap_or_default();
        // Some drivers keep sensor files under device/.
        let base = if has_inputs(&dir) { dir.clone() } else { dir.join("device") };
        out.extend(read_sensors(&base, &chip, &chip_name));
    }
    out
}

fn has_inputs(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|it| it.flatten().any(|e| e.file_name().to_string_lossy().ends_with("_input")))
        .unwrap_or(false)
}

fn read_sensors(dir: &Path, chip: &str, chip_name: &str) -> Vec<Sensor> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    let mut names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    for name in names {
        let Some(sensor) = name.strip_suffix("_input") else { continue };
        let (kind, scale) = if sensor.starts_with("temp") {
            (SensorKind::Temp, 1000.0)
        } else if sensor.starts_with("fan") {
            (SensorKind::Fan, 1.0)
        } else {
            continue;
        };
        let Some(raw) = read(dir.join(&name)).and_then(|s| s.trim().parse::<f64>().ok()) else { continue };
        let opt = |suffix: &str| read_u64(dir.join(format!("{sensor}_{suffix}"))).map(|v| v as f64 / scale).filter(|v| *v > 0.0);
        out.push(Sensor {
            chip: chip.to_string(),
            chip_name: chip_name.to_string(),
            sensor: sensor.to_string(),
            label: read(dir.join(format!("{sensor}_label"))).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            kind,
            value: raw / scale,
            max: if kind == SensorKind::Temp { opt("max") } else { None },
            crit: if kind == SensorKind::Temp { opt("crit") } else { None },
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn cleans_like_node_exporter() {
        assert_eq!(clean("0000:00:18.3"), "0000:00:18_3");
        assert_eq!(clean("nct6683.2592"), "nct6683_2592");
        assert_eq!(clean("r8169_0_2a00:00\n"), "r8169_0_2a00:00");
    }

    #[test]
    fn reads_tree() {
        let root = std::env::temp_dir().join(format!("collector-hwmon-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dev = root.join("devices/pci0000:00/0000:00:18.3");
        let hw = root.join("class/hwmon/hwmon3");
        fs::create_dir_all(&dev).unwrap();
        fs::create_dir_all(&hw).unwrap();
        std::os::unix::fs::symlink(&dev, hw.join("device")).unwrap();
        fs::write(hw.join("name"), "k10temp\n").unwrap();
        fs::write(hw.join("temp1_input"), "72500\n").unwrap();
        fs::write(hw.join("temp1_label"), "Tctl\n").unwrap();
        fs::write(hw.join("temp1_crit"), "95000\n").unwrap();
        fs::write(hw.join("fan2_input"), "1200\n").unwrap();
        fs::write(hw.join("in0_input"), "1100\n").unwrap();

        let nvme = root.join("class/hwmon/hwmon0");
        fs::create_dir_all(&nvme).unwrap();
        fs::write(nvme.join("name"), "nvme\n").unwrap();
        fs::write(nvme.join("temp1_input"), "37850\n").unwrap();

        let s = collect(&root);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].chip, "nvme");
        assert_eq!(s[0].value, 37.85);
        let t = s.iter().find(|x| x.sensor == "temp1" && x.chip_name == "k10temp").unwrap();
        assert_eq!(t.chip, "pci0000:00_0000:00:18_3");
        assert_eq!(t.label.as_deref(), Some("Tctl"));
        assert_eq!(t.value, 72.5);
        assert_eq!(t.crit, Some(95.0));
        let f = s.iter().find(|x| x.kind == SensorKind::Fan).unwrap();
        assert_eq!(f.value, 1200.0);
        let _ = fs::remove_dir_all(&root);
    }
}
