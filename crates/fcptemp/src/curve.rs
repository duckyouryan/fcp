use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Curve {
    Linear {
        min_temp_c: f64,
        max_temp_c: f64,
        min_throttle: u8,
        max_throttle: u8,
    },
    Power {
        min_temp_c: f64,
        max_temp_c: f64,
        min_throttle: u8,
        max_throttle: u8,
        exponent: f64,
    },
    Points {
        min_temp_c: f64,
        max_temp_c: f64,
        min_throttle: u8,
        max_throttle: u8,
        points: Vec<CurvePoint>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurvePoint {
    pub temp_c: f64,
    pub throttle: u8,
}

impl Curve {
    pub fn validate(&self) -> Result<(), String> {
        let (min_temp, max_temp, min_throttle, max_throttle) = self.bounds();
        if !min_temp.is_finite() || !max_temp.is_finite() || min_temp >= max_temp {
            return Err("curve requires finite temperatures with min_temp_c < max_temp_c".to_owned());
        }
        if min_throttle > 100 || max_throttle > 100 || min_throttle > max_throttle {
            return Err("curve requires 0 <= min_throttle <= max_throttle <= 100".to_owned());
        }

        match self {
            Self::Linear { .. } => {}
            Self::Power { exponent, .. } => {
                if !exponent.is_finite() || !(0.25..=8.0).contains(exponent) {
                    return Err("power curve exponent must be between 0.25 and 8.0".to_owned());
                }
            }
            Self::Points { points, .. } => {
                if points.len() > 8 {
                    return Err("points curve allows at most 8 intermediate points".to_owned());
                }
                let mut previous_temp = min_temp;
                let mut previous_throttle = min_throttle;
                for (index, point) in points.iter().enumerate() {
                    if !point.temp_c.is_finite()
                        || point.temp_c <= previous_temp
                        || point.temp_c >= max_temp
                    {
                        return Err(format!(
                            "point {} temperature must be finite and strictly between the previous point and max_temp_c",
                            index + 1
                        ));
                    }
                    if point.throttle < previous_throttle || point.throttle > max_throttle {
                        return Err(format!(
                            "point {} throttle must not decrease and must not exceed max_throttle",
                            index + 1
                        ));
                    }
                    previous_temp = point.temp_c;
                    previous_throttle = point.throttle;
                }
            }
        }
        Ok(())
    }

    pub fn calculate(&self, temp_c: f64) -> u8 {
        let (min_temp, max_temp, min_throttle, max_throttle) = self.bounds();
        if temp_c <= min_temp {
            return min_throttle;
        }
        if temp_c >= max_temp {
            return max_throttle;
        }

        match self {
            Self::Linear { .. } => {
                let x = (temp_c - min_temp) / (max_temp - min_temp);
                interpolate(min_throttle as f64, max_throttle as f64, x)
            }
            Self::Power { exponent, .. } => {
                let x = (temp_c - min_temp) / (max_temp - min_temp);
                interpolate(min_throttle as f64, max_throttle as f64, x.powf(*exponent))
            }
            Self::Points { points, .. } => {
                let mut left_temp = min_temp;
                let mut left_throttle = min_throttle;
                for point in points {
                    if temp_c <= point.temp_c {
                        let x = (temp_c - left_temp) / (point.temp_c - left_temp);
                        return interpolate(left_throttle as f64, point.throttle as f64, x);
                    }
                    left_temp = point.temp_c;
                    left_throttle = point.throttle;
                }
                let x = (temp_c - left_temp) / (max_temp - left_temp);
                interpolate(left_throttle as f64, max_throttle as f64, x)
            }
        }
    }

    pub fn bounds(&self) -> (f64, f64, u8, u8) {
        match self {
            Self::Linear { min_temp_c, max_temp_c, min_throttle, max_throttle }
            | Self::Power { min_temp_c, max_temp_c, min_throttle, max_throttle, .. }
            | Self::Points { min_temp_c, max_temp_c, min_throttle, max_throttle, .. } => {
                (*min_temp_c, *max_temp_c, *min_throttle, *max_throttle)
            }
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Linear { .. } => "linear",
            Self::Power { .. } => "power",
            Self::Points { .. } => "points",
        }
    }
}

fn interpolate(start: f64, end: f64, fraction: f64) -> u8 {
    (start + (end - start) * fraction.clamp(0.0, 1.0))
        .round()
        .clamp(0.0, 100.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_curve_preserves_endpoints() {
        let curve = Curve::Linear {
            min_temp_c: 30.0,
            max_temp_c: 70.0,
            min_throttle: 20,
            max_throttle: 100,
        };
        assert_eq!(curve.calculate(30.0), 20);
        assert_eq!(curve.calculate(50.0), 60);
        assert_eq!(curve.calculate(70.0), 100);
    }

    #[test]
    fn power_curve_uses_exponent_to_shape_ramp() {
        let curve = Curve::Power {
            min_temp_c: 30.0,
            max_temp_c: 70.0,
            min_throttle: 20,
            max_throttle: 100,
            exponent: 2.0,
        };
        assert_eq!(curve.calculate(30.0), 20);
        assert_eq!(curve.calculate(50.0), 40);
        assert_eq!(curve.calculate(70.0), 100);
    }

    #[test]
    fn point_curve_interpolates_between_milestones() {
        let curve = Curve::Points {
            min_temp_c: 30.0,
            max_temp_c: 70.0,
            min_throttle: 20,
            max_throttle: 100,
            points: vec![CurvePoint { temp_c: 40.0, throttle: 50 }],
        };
        assert_eq!(curve.calculate(35.0), 35);
        assert_eq!(curve.calculate(40.0), 50);
        assert_eq!(curve.calculate(55.0), 75);
    }

    #[test]
    fn point_curve_rejects_decreasing_throttle() {
        let curve = Curve::Points {
            min_temp_c: 30.0,
            max_temp_c: 70.0,
            min_throttle: 20,
            max_throttle: 100,
            points: vec![
                CurvePoint { temp_c: 40.0, throttle: 60 },
                CurvePoint { temp_c: 50.0, throttle: 55 },
            ],
        };
        assert!(curve.validate().is_err());
    }
}
