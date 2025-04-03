pub fn read_offset(offset_dir: &str) -> anyhow::Result<i64> {
    let offset = std::fs::read_to_string(format!("{}/offset", offset_dir))?;
    let offset = offset.trim().parse()?;
    Ok(offset)
}

pub fn write_offset(offset_dir: &str, offset: i64) -> anyhow::Result<()> {
    std::fs::create_dir_all(offset_dir)?;
    std::fs::write(format!("{}/offset.tmp", offset_dir), offset.to_string())?;
    std::fs::rename(
        format!("{}/offset.tmp", offset_dir),
        format!("{}/offset", offset_dir),
    )?;
    Ok(())
}
