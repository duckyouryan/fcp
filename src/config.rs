//! Config file: plain text, one directive per line, `#` starts a comment.
//! `fan NAME` opens a block; pwm/tach/sensor/curve lines belong to the last fan.

use std::fs;
use std::path::PathBuf;

pub struct Config {
    pub frequency: f64,
    pub fans: Vec<FanCfg>,
}

pub struct FanCfg {
    pub name: String,
    pub pwm: Option<Pwm>,
    pub tach: Option<Src>,
    pub sensor: Option<Src>,
    /// (°C, percent) sorted by temperature. Empty = no auto mode.
    pub curve: Vec<(f64, f64)>,
}

/// The file we write to, and which value in it means slowest / fastest.
pub struct Pwm {
    pub src: Src,
    pub min: i64,
    pub max: i64,
}

pub struct Src {
    pub path: PathBuf,
    /// If set, `<dir>/name` must contain this, else we refuse to start.
    pub chip: Option<String>,
}

impl Src {
    pub fn check(&self) -> Result<(), String> {
        fs::metadata(&self.path).map_err(|e| format!("{}: {e}", self.path.display()))?;
        let Some(want) = &self.chip else { return Ok(()) };
        let name_file = self.path.parent().unwrap_or(&self.path).join("name");
        let got = fs::read_to_string(&name_file)
            .map_err(|e| format!("cannot read {}: {e}", name_file.display()))?;
        if got.trim() != want {
            return Err(format!(
                "{} belongs to '{}', but the config expects '{want}'",
                self.path.display(),
                got.trim()
            ));
        }
        Ok(())
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Config, String> {
        let mut cfg = Config { frequency: 10.0, fans: Vec::new() };
        for (n, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("");
            let mut words = line.split_whitespace();
            let Some(key) = words.next() else { continue };
            let args: Vec<&str> = words.collect();
            parse_line(&mut cfg, key, &args).map_err(|e| format!("line {}: {e}", n + 1))?;
        }
        if cfg.fans.is_empty() {
            return Err("no fans configured".into());
        }
        for f in &cfg.fans {
            if f.sensor.is_some() == f.curve.is_empty() {
                return Err(format!("fan {}: 'sensor' and 'curve' go together", f.name));
            }
        }
        Ok(cfg)
    }

    /// Touch the filesystem: every path must exist and belong to the right chip.
    pub fn check(&self) -> Result<(), String> {
        for f in &self.fans {
            let srcs = f.pwm.iter().map(|p| &p.src).chain(&f.tach).chain(&f.sensor);
            for s in srcs {
                s.check().map_err(|e| format!("fan {}: {e}", f.name))?;
            }
        }
        Ok(())
    }
}

fn parse_line(cfg: &mut Config, key: &str, args: &[&str]) -> Result<(), String> {
    match key {
        "frequency" => {
            let f: f64 = one(args)?.parse().map_err(|_| "frequency must be a number")?;
            if !(0.1..=1000.0).contains(&f) {
                return Err("frequency must be between 0.1 and 1000".into());
            }
            cfg.frequency = f;
        }
        "fan" => {
            let name = one(args)?;
            if cfg.fans.iter().any(|f| f.name == name) {
                return Err(format!("duplicate fan '{name}'"));
            }
            cfg.fans.push(FanCfg {
                name: name.to_string(),
                pwm: None,
                tach: None,
                sensor: None,
                curve: Vec::new(),
            });
        }
        _ => {
            let fan = cfg.fans.last_mut().ok_or_else(|| format!("'{key}' before any 'fan' line"))?;
            match key {
                "pwm" => {
                    let (src, min, max) = src(args, true)?;
                    let (Some(min), Some(max)) = (min, max) else {
                        return Err("pwm needs min=N and max=N".into());
                    };
                    if min == max {
                        return Err("pwm min and max must differ".into());
                    }
                    fan.pwm = Some(Pwm { src, min, max });
                }
                "tach" => fan.tach = Some(src(args, false)?.0),
                "sensor" => fan.sensor = Some(src(args, false)?.0),
                "curve" => fan.curve = curve(args)?,
                _ => return Err(format!("unknown directive '{key}'")),
            }
        }
    }
    Ok(())
}

fn one<'a>(args: &[&'a str]) -> Result<&'a str, String> {
    match args {
        [one] => Ok(one),
        _ => Err("expected exactly one argument".into()),
    }
}

fn src(args: &[&str], levels: bool) -> Result<(Src, Option<i64>, Option<i64>), String> {
    let (path, opts) = args.split_first().ok_or("missing path")?;
    if !path.starts_with('/') {
        return Err(format!("'{path}' is not an absolute path"));
    }
    let mut src = Src { path: PathBuf::from(path), chip: None };
    let (mut min, mut max) = (None, None);
    for opt in opts {
        let int = |v: &str| v.parse::<i64>().map_err(|_| format!("bad number in '{opt}'"));
        match opt.split_once('=') {
            Some(("chip", v)) => src.chip = Some(v.to_string()),
            Some(("min", v)) if levels => min = Some(int(v)?),
            Some(("max", v)) if levels => max = Some(int(v)?),
            _ => return Err(format!("unknown option '{opt}'")),
        }
    }
    Ok((src, min, max))
}

fn curve(args: &[&str]) -> Result<Vec<(f64, f64)>, String> {
    if args.is_empty() {
        return Err("curve needs points like 40:0 75:100".into());
    }
    let mut points: Vec<(f64, f64)> = Vec::new();
    for a in args {
        let (t, p) = a.split_once(':').ok_or_else(|| format!("'{a}' is not TEMP:PERCENT"))?;
        let t: f64 = t.parse().map_err(|_| format!("bad temperature in '{a}'"))?;
        let p: f64 = p.parse().map_err(|_| format!("bad percent in '{a}'"))?;
        if !t.is_finite() || !(0.0..=100.0).contains(&p) {
            return Err(format!("'{a}': percent must be 0..100"));
        }
        if points.last().is_some_and(|last| t <= last.0) {
            return Err("curve temperatures must increase".into());
        }
        points.push((t, p));
    }
    Ok(points)
}

/// Percent for a temperature: linear between points, flat outside them.
pub fn eval(curve: &[(f64, f64)], t: f64) -> u8 {
    let (Some(first), Some(last)) = (curve.first(), curve.last()) else { return 100 };
    if t <= first.0 {
        return first.1.round() as u8;
    }
    for w in curve.windows(2) {
        let (a, b) = (w[0], w[1]);
        if t <= b.0 {
            return (a.1 + (b.1 - a.1) * (t - a.0) / (b.0 - a.0)).round() as u8;
        }
    }
    last.1.round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "
        frequency 20
        fan A   # comment
            pwm    /x/pwm1_enable chip=it8792 min=2 max=0
            tach   /x/fan1_input
            sensor /y/temp1_input chip=k10temp
            curve  40:0 75:100
        fan B
            tach /x/fan2_input
    ";

    #[test]
    fn parses_a_full_config() {
        let cfg = Config::parse(GOOD).unwrap();
        assert_eq!(cfg.frequency, 20.0);
        assert_eq!(cfg.fans.len(), 2);
        let a = &cfg.fans[0];
        let pwm = a.pwm.as_ref().unwrap();
        assert_eq!((pwm.min, pwm.max), (2, 0));
        assert_eq!(pwm.src.chip.as_deref(), Some("it8792"));
        assert_eq!(a.curve, vec![(40.0, 0.0), (75.0, 100.0)]);
        assert!(cfg.fans[1].pwm.is_none());
    }

    #[test]
    fn frequency_defaults_to_10() {
        assert_eq!(Config::parse("fan A\n tach /x").unwrap().frequency, 10.0);
    }

    #[test]
    fn rejects_bad_configs() {
        let bad = [
            "",                                                      // no fans
            "pwm /x min=2 max=0",                                    // before any fan
            "fan A\n pwm /x min=2",                                  // max missing
            "fan A\n pwm /x min=2 max=2",                            // identical
            "fan A\n pwm x/y min=2 max=0",                           // relative path
            "fan A\n tach /x min=2",                                 // min only for pwm
            "fan A\n sensor /x",                                     // sensor without curve
            "fan A\n curve 40:0 30:100",                             // temps not increasing
            "fan A\n curve 40:0 50:101",                             // percent out of range
            "fan A\n bogus 1",                                       // unknown directive
            "fan A\n fan A",                                         // duplicate
            "frequency 0\n fan A",                                   // frequency out of range
        ];
        for text in bad {
            assert!(Config::parse(text).is_err(), "should be rejected: {text:?}");
        }
    }

    #[test]
    fn curve_interpolates_and_clamps() {
        let c = [(40.0, 0.0), (75.0, 100.0)];
        assert_eq!(eval(&c, 20.0), 0);
        assert_eq!(eval(&c, 40.0), 0);
        assert_eq!(eval(&c, 57.5), 50);
        assert_eq!(eval(&c, 75.0), 100);
        assert_eq!(eval(&c, 99.0), 100);
        let c = [(30.0, 10.0), (50.0, 20.0), (70.0, 100.0)];
        assert_eq!(eval(&c, 60.0), 60);
    }
}
