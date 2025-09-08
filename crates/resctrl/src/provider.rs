use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub trait FsProvider: Clone + Send + Sync + 'static {
    fn exists(&self, p: &Path) -> bool;
    fn create_dir(&self, p: &Path) -> io::Result<()>;
    fn remove_dir(&self, p: &Path) -> io::Result<()>;
    fn write_str(&self, p: &Path, data: &str) -> io::Result<()>;
    fn read_to_string(&self, p: &Path) -> io::Result<String>;
}

#[derive(Clone, Copy, Debug)]
pub struct RealFs;

impl FsProvider for RealFs {
    fn exists(&self, p: &Path) -> bool {
        p.exists()
    }

    fn create_dir(&self, p: &Path) -> io::Result<()> {
        fs::create_dir(p)
    }

    fn remove_dir(&self, p: &Path) -> io::Result<()> {
        fs::remove_dir(p)
    }

    fn write_str(&self, p: &Path, data: &str) -> io::Result<()> {
        // For resctrl tasks, the file must exist; do not create.
        let mut f = OpenOptions::new().write(true).open(p)?;
        f.write_all(data.as_bytes())
    }

    fn read_to_string(&self, p: &Path) -> io::Result<String> {
        fs::read_to_string(p)
    }
}

