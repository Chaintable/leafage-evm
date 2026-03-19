pub mod table;
pub mod report;

pub(crate) use table::TableView;

pub(crate) fn format_delta_percent(base: f64, new_value: f64) -> String {
    if base.abs() < f64::EPSILON {
        return "-".to_string();
    }
    let delta = (new_value - base) / base * 100.0;
    format!("{:+.2}%", delta)
}

pub(crate) fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.iter().sum::<f64>() / v.len() as f64
}

pub(crate) fn stddev(v: &[f64]) -> f64 {
    if v.len() < 2 {
        return 0.0;
    }
    let m = mean(v);
    let variance = v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (v.len() - 1) as f64;
    variance.sqrt()
}

pub(crate) fn fmt_mean_std(v: &[f64]) -> String {
    format!("{:.2} ± {:.2}", mean(v), stddev(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_empty() {
        assert_eq!(mean(&[]), 0.0);
    }

    #[test]
    fn mean_single() {
        assert!((mean(&[42.0]) - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn mean_known() {
        // (1 + 2 + 3 + 4 + 5) / 5 = 3.0
        assert!((mean(&[1.0, 2.0, 3.0, 4.0, 5.0]) - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn mean_negative() {
        assert!((mean(&[-2.0, 0.0, 2.0]) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn stddev_empty() {
        assert_eq!(stddev(&[]), 0.0);
    }

    #[test]
    fn stddev_single() {
        // With < 2 samples, stddev is defined as 0
        assert_eq!(stddev(&[99.0]), 0.0);
    }

    #[test]
    fn stddev_identical() {
        assert_eq!(stddev(&[5.0, 5.0, 5.0]), 0.0);
    }

    #[test]
    fn stddev_known() {
        // Sample: [2, 4, 4, 4, 5, 5, 7, 9]
        // Mean = 5.0, sample variance = 4.571..., stddev ≈ 2.1380899...
        let v = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        let s = stddev(&v);
        assert!((s - 2.138_089_935_299_395).abs() < 1e-10);
    }

    #[test]
    fn stddev_two_elements() {
        // [0, 10], mean = 5, variance = (25+25)/1 = 50, stddev = sqrt(50)
        let s = stddev(&[0.0, 10.0]);
        assert!((s - 50.0_f64.sqrt()).abs() < 1e-10);
    }

    #[test]
    fn delta_base_zero() {
        assert_eq!(format_delta_percent(0.0, 100.0), "-");
    }

    #[test]
    fn delta_base_near_zero() {
        assert_eq!(format_delta_percent(1e-20, 1.0), "-");
    }

    #[test]
    fn delta_no_change() {
        assert_eq!(format_delta_percent(100.0, 100.0), "+0.00%");
    }

    #[test]
    fn delta_positive() {
        // (200 - 100) / 100 * 100 = +100%
        assert_eq!(format_delta_percent(100.0, 200.0), "+100.00%");
    }

    #[test]
    fn delta_negative() {
        // (50 - 100) / 100 * 100 = -50%
        assert_eq!(format_delta_percent(100.0, 50.0), "-50.00%");
    }

    #[test]
    fn delta_fractional() {
        // (105 - 100) / 100 * 100 = +5%
        assert_eq!(format_delta_percent(100.0, 105.0), "+5.00%");
    }


    #[test]
    fn fmt_mean_std_empty() {
        assert_eq!(fmt_mean_std(&[]), "0.00 ± 0.00");
    }

    #[test]
    fn fmt_mean_std_single() {
        assert_eq!(fmt_mean_std(&[3.14]), "3.14 ± 0.00");
    }

    #[test]
    fn fmt_mean_std_known() {
        // [10, 20]: mean=15, stddev=sqrt(50)≈7.07
        let s = fmt_mean_std(&[10.0, 20.0]);
        assert!(s.starts_with("15.00 ± 7.07"));
    }
}
