use anyhow::{bail, Result};
#[derive(clap::Args)] pub struct CalendarsArgs {}
#[derive(clap::Args)] pub struct JsonFlag {}
#[derive(clap::Args)] pub struct LsArgs {}
pub fn calendars(_: CalendarsArgs) -> Result<()> { bail!("stub") }
pub fn add(_: &str) -> Result<()> { bail!("stub") }
pub fn today(_: JsonFlag) -> Result<()> { bail!("stub") }
pub fn next(_: JsonFlag) -> Result<()> { bail!("stub") }
pub fn done(_: i64) -> Result<()> { bail!("stub") }
pub fn reopen(_: i64) -> Result<()> { bail!("stub") }
pub fn rm(_: i64) -> Result<()> { bail!("stub") }
pub fn restore(_: i64) -> Result<()> { bail!("stub") }
pub fn ls(_: LsArgs) -> Result<()> { bail!("stub") }
