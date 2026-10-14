// Copyright 2026 Jakub Hlavnicka
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Gives the Windows .exe its icon: Explorer, the desktop and the taskbar read
//! it from the file's resources, before the program has run (the window icon
//! set in main.rs only exists while it runs). mingw's windres compiles the .rc
//! into an object that is linked into the binaries.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=packaging/windows/resynth-studio.rc");
    println!("cargo:rerun-if-changed=packaging/icon/resynth-studio.ico");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // Cross builds from Linux use the triple-prefixed windres; a native mingw
    // has the plain one. WINDRES overrides both.
    let windres = env::var("WINDRES").unwrap_or_else(|_| {
        if env::var("HOST") == env::var("TARGET") {
            "windres".to_string()
        } else {
            "x86_64-w64-mingw32-windres".to_string()
        }
    });
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("resynth-studio-rc.o");
    let status = Command::new(&windres)
        .args(["--input-format=rc", "--output-format=coff", "-I", "packaging/icon", "-i"])
        .arg("packaging/windows/resynth-studio.rc")
        .arg("-o")
        .arg(&out)
        .status()
        .unwrap_or_else(|e| panic!("could not run {windres}: {e}"));
    assert!(status.success(), "{windres} failed on packaging/windows/resynth-studio.rc");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}
