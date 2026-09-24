//! Embeds the icon and version information into the executable.
//!
//! The tray icon is drawn at runtime, which is enough for the notification area
//! but not for Explorer, the Start menu or the taskbar button: those read the
//! icon straight out of the executable's resources. The icon embedded here is
//! byte-for-byte the same `assets/icon.ico` the installer uses — both come from
//! `xshot --export-icon`, which is the drawing code itself.
//!
//! `rc.exe` (Windows SDK) has to be on `PATH`. Without it the build still
//! succeeds, it just falls back to the default application icon.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(windows)]
    if let Err(error) = embed() {
        println!("cargo:warning=not embedding the application icon: {error}");
    }
}

#[cfg(windows)]
fn embed() -> Result<(), Box<dyn std::error::Error>> {
    use std::fmt::Write;

    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let icon = manifest.join("assets").join("icon.ico");
    if !icon.exists() {
        return Err("assets/icon.ico is missing — run `xshot --export-icon assets/icon.ico`".into());
    }

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let mut parts = version.split('.').map(|p| p.parse::<u16>().unwrap_or(0));
    let (major, minor, patch) = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );

    let mut rc = String::new();
    // RC reads backslash escapes inside strings, so `C:\Users\22357` would be
    // mangled into an octal escape. Double every one of them.
    let icon_path = icon.display().to_string().replace('\\', "\\\\");
    writeln!(rc, "1 ICON \"{icon_path}\"")?;
    writeln!(rc, "1 VERSIONINFO")?;
    writeln!(rc, "FILEVERSION {major},{minor},{patch},0")?;
    writeln!(rc, "PRODUCTVERSION {major},{minor},{patch},0")?;
    writeln!(rc, "FILEOS 0x40004")?;
    writeln!(rc, "FILETYPE 0x1")?;
    writeln!(rc, "BEGIN")?;
    writeln!(rc, "  BLOCK \"StringFileInfo\"")?;
    writeln!(rc, "  BEGIN")?;
    // 080404B0: Simplified Chinese, Unicode.
    writeln!(rc, "    BLOCK \"080404B0\"")?;
    writeln!(rc, "    BEGIN")?;
    writeln!(
        rc,
        "      VALUE \"FileDescription\", \"A lean, fast Windows screenshot tool\""
    )?;
    writeln!(rc, "      VALUE \"FileVersion\", \"{version}\"")?;
    writeln!(rc, "      VALUE \"ProductName\", \"xshot\"")?;
    writeln!(rc, "      VALUE \"ProductVersion\", \"{version}\"")?;
    writeln!(rc, "      VALUE \"OriginalFilename\", \"xshot.exe\"")?;
    writeln!(rc, "    END")?;
    writeln!(rc, "  END")?;
    writeln!(rc, "  BLOCK \"VarFileInfo\"")?;
    writeln!(rc, "  BEGIN")?;
    writeln!(rc, "    VALUE \"Translation\", 0x804, 1200")?;
    writeln!(rc, "  END")?;
    writeln!(rc, "END")?;

    let rc_path = out_dir.join("xshot.rc");
    std::fs::write(&rc_path, rc)?;
    let res_path = out_dir.join("xshot.res");
    let status = Command::new("rc.exe")
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_path)
        .arg(&rc_path)
        .status()?;
    if !status.success() {
        return Err(format!("rc.exe failed with {status}").into());
    }
    println!("cargo:rustc-link-arg={}", res_path.display());
    Ok(())
}
