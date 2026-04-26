use embed_manifest::{embed_manifest, manifest::ExecutionLevel, new_manifest};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    copy_tray_icon();

    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // DPI-aware, no UAC, no console window.
        embed_manifest(
            new_manifest("Tempix.Hud")
                .requested_execution_level(ExecutionLevel::RequireAdministrator),
        )
        .expect("unable to embed manifest");

        if std::env::var("PROFILE").as_deref() == Ok("release") {
            publish_sensor_helper();
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/tempix.ico");
    println!("cargo:rerun-if-changed=sensor-helper/TempixSensorHelper.csproj");
    println!("cargo:rerun-if-changed=sensor-helper/Program.cs");
}

fn copy_tray_icon() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let icon_src = manifest_dir.join("assets").join("tempix.ico");
    if !icon_src.exists() {
        return;
    }

    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let output_dir = manifest_dir.join("target").join(profile);
    let _ = fs::create_dir_all(&output_dir);
    let _ = fs::copy(icon_src, output_dir.join("tempix.ico"));
}

fn publish_sensor_helper() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let project = manifest_dir
        .join("sensor-helper")
        .join("TempixSensorHelper.csproj");
    let publish_dir = manifest_dir.join("target").join("release");

    let status = Command::new("dotnet")
        .arg("publish")
        .arg(project)
        .arg("-c")
        .arg("Release")
        .arg("-r")
        .arg("win-x64")
        .arg("--self-contained")
        .arg("false")
        .arg("-p:PublishSingleFile=true")
        .arg(format!("-p:PublishDir={}", publish_dir.display()))
        .status()
        .expect("unable to run dotnet publish for tempix-sensors");

    if !status.success() {
        panic!("dotnet publish failed for tempix-sensors");
    }
}
