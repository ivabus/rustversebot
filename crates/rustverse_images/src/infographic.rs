//! Render one model-authored Typst page in a fresh project. No remote assets are staged.
use std::{ffi::OsString, path::Path, process::Stdio, time::Duration};

use anyhow::{Context, Result, ensure};
use tokio::{fs, io::AsyncReadExt, process::Command};

const MAX_FILE: u64 = 12 * 1024 * 1024;

#[cfg(unix)]
fn limit_child(command: &mut Command) {
    // Only async-signal-safe calls between fork and exec. Bound the work of both tools.
    unsafe {
        command.pre_exec(|| {
            let limits = [
                (libc::RLIMIT_CPU, 25),
                (libc::RLIMIT_FSIZE, MAX_FILE as libc::rlim_t),
                #[cfg(target_os = "linux")]
                (libc::RLIMIT_AS, 1536 * 1024 * 1024),
            ];
            for (resource, ceiling) in limits {
                let mut limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::getrlimit(resource, &mut limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                limit.rlim_cur = limit.rlim_cur.min(ceiling);
                limit.rlim_max = limit.rlim_cur;
                if libc::setrlimit(resource, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

// Darwin does not implement an address-space rlimit. Check resident memory
// while waiting for the child; kill_on_drop also covers cancellation here.
#[cfg(target_os = "macos")]
async fn wait_child(child: &mut tokio::process::Child) -> Result<std::process::ExitStatus> {
    let pid = child.id().context("Missing renderer PID")? as i32;
    loop {
        tokio::select! {
            status = child.wait() => return Ok(status?),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                let mut info=std::mem::MaybeUninit::<libc::proc_taskinfo>::uninit();
                let size=std::mem::size_of::<libc::proc_taskinfo>() as i32;
                // proc_pidinfo initializes the full structure on a full-size return.
                let used=unsafe { libc::proc_pidinfo(pid,libc::PROC_PIDTASKINFO,0,info.as_mut_ptr().cast(),size) };
                if used==size {
                    ensure!(unsafe {info.assume_init()}.pti_resident_size < 1536*1024*1024,"Рендеринг превысил лимит памяти");
                }
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
async fn wait_child(child: &mut tokio::process::Child) -> Result<std::process::ExitStatus> {
    Ok(child.wait().await?)
}

async fn run(bin: OsString, args: &[&str], directory: &Path) -> Result<()> {
    let log = directory.join("diagnostic.log");
    let mut command = Command::new(bin);
    command
        .env_clear()
        .current_dir(directory)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(&log).await?.into_std().await)
        .kill_on_drop(true);
    // Keep system font discovery, including Maple Mono installed for the service user.
    for key in [
        "PATH",
        "HOME",
        "FONTCONFIG_FILE",
        "FONTCONFIG_PATH",
        "TYPST_FONT_PATHS",
        "SYSTEMROOT",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    #[cfg(unix)]
    limit_child(&mut command);
    let mut child = command.spawn().context("Не удалось запустить рендерер")?;
    let status = tokio::time::timeout(Duration::from_secs(30), wait_child(&mut child))
        .await
        .context("Превышено время рендеринга")??;
    if !status.success() {
        let mut bytes = Vec::new();
        fs::File::open(log)
            .await?
            .take(1600)
            .read_to_end(&mut bytes)
            .await?;
        anyhow::bail!("Ошибка вёрстки: {}", String::from_utf8_lossy(&bytes));
    }
    Ok(())
}

fn validate_svg(svg: &str) -> Result<()> {
    let doc = roxmltree::Document::parse_with_options(
        svg,
        roxmltree::ParsingOptions {
            nodes_limit: 150_000,
            ..Default::default()
        },
    )?;
    let root = doc.root_element();
    ensure!(root.tag_name().name() == "svg", "Expected SVG");
    let bounds: Vec<f64> = root
        .attribute("viewBox")
        .context("Missing SVG bounds")?
        .split_ascii_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()?;
    ensure!(
        bounds.len() == 4
            && bounds[2] > 0.
            && bounds[3] > 0.
            && (0.2..=2.5).contains(&(bounds[3] / bounds[2])),
        "Измени пропорции страницы: высота должна быть от 0,2 до 2,5 ширины"
    );
    for node in doc.descendants().filter(|n| n.is_element()) {
        for attr in node.attributes().filter(|a| a.name() == "href") {
            let value = attr.value();
            ensure!(
                value.starts_with('#')
                    || value.starts_with("data:image/png;base64,")
                    || value.starts_with("data:image/jpeg;base64,"),
                "Используй встроенные фигуры и текст вместо внешних ссылок или изображений"
            );
        }
    }
    Ok(())
}

pub async fn render_infographic(source: String) -> Result<Vec<u8>> {
    ensure!(
        !source.trim().is_empty() && source.len() <= 64 * 1024,
        "Нужен одностраничный макет до 64 КиБ"
    );
    let temp = tempfile::tempdir()?;
    let dir = temp.path();
    let project = dir.join("project");
    fs::create_dir(&project).await?;
    fs::create_dir(dir.join("packages")).await?;
    fs::write(
        project.join("prelude.typ"),
        include_bytes!("../prelude.typ"),
    )
    .await?;
    fs::write(project.join("inpin.ttf"), include_bytes!("../inpin.ttf")).await?;
    fs::write(project.join("layout.typ"), source).await?;
    fs::write(
        project.join("main.typ"),
        r##"#import "prelude.typ": conf
#show: conf.with(mode: "diff", size: "big", group-digits: false)
#set text(lang: "ru", font: ("Maple Mono", "inpin hongmengti"), size: 10pt)
#include "layout.typ"
"##,
    )
    .await?;
    // An isolated, empty package store and an unreadable download certificate make
    // Typst package resolution fail locally, before its downloader can issue a request.
    run(
        std::env::var_os("TYPST_BIN").unwrap_or_else(|| "typst".into()),
        &[
            "--cert",
            "packages",
            "compile",
            "--root",
            "project",
            "--font-path",
            "project",
            "--package-path",
            "packages",
            "--package-cache-path",
            "packages",
            "--diagnostic-format",
            "short",
            "-j",
            "1",
            "--format",
            "svg",
            "project/main.typ",
            "page.svg",
        ],
        dir,
    )
    .await?;
    let path = dir.join("page.svg");
    ensure!(
        fs::metadata(&path).await?.len() <= MAX_FILE,
        "SVG слишком большой"
    );
    let svg = fs::read_to_string(path).await?;
    tokio::task::spawn_blocking(move || validate_svg(&svg)).await??;
    run(
        std::env::var_os("RESVG_BIN").unwrap_or_else(|| "resvg".into()),
        &[
            "--width",
            super::HIRES_WIDTH,
            "--resources-dir",
            "packages",
            "page.svg",
            "page.png",
        ],
        dir,
    )
    .await?;
    let path = dir.join("page.png");
    ensure!(
        fs::metadata(&path).await?.len() <= MAX_FILE,
        "PNG слишком большой"
    );
    let png = fs::read(path).await?;
    ensure!(
        png.len() >= 24
            && png.starts_with(b"\x89PNG\r\n\x1a\n")
            && u32::from_be_bytes(png[16..20].try_into()?) == 2560,
        "Некорректный PNG"
    );
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn renders_free_layout_at_delivery_resolution_and_reports_typst_errors() {
        let png=render_infographic("#set page(width: 15cm, height: 10cm)\n#align(center)[#text(size: 18pt)[Штурм]]\n#grid(columns: (1fr, 1fr), [Обычный этаж], [Сложный этаж])".into()).await.unwrap();
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 2560);
        assert!((1705..1710).contains(&u32::from_be_bytes(png[20..24].try_into().unwrap())));
        assert!(
            render_infographic("#unknown_function()".into())
                .await
                .is_err()
        );
    }
}
