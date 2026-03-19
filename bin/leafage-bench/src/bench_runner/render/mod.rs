
pub mod table;
pub mod report;

pub(crate) use table::Render;
pub(crate) use report::Report;

fn format_delta_percent(base: f64, new_value: f64) -> String {
    if base.abs() < f64::EPSILON {
        return "-".to_string();
    }
    let delta = (new_value - base) / base * 100.0;
    format!("{:+.2}%", delta)
}
fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.iter().sum::<f64>() / v.len() as f64
}

fn stddev(v: &[f64]) -> f64 {
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