use std::path::PathBuf;

pub struct Dirs {
    pub root: PathBuf,
    pub config: PathBuf,
    pub db: PathBuf,
    pub webview: PathBuf,
    pub logs: PathBuf,
}

/// %APPDATA%\review-monitor\ 下的各个路径（不存在就创建）
pub fn ensure() -> anyhow::Result<Dirs> {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    let root = base.join("review-monitor");
    let d = Dirs {
        config: root.join("config.toml"),
        db: root.join("data.db"),
        webview: root.join("webview"),
        logs: root.join("logs"),
        root,
    };
    for p in [&d.root, &d.webview, &d.logs] {
        std::fs::create_dir_all(p)?;
    }
    Ok(d)
}
