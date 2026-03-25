use std::path::{Path, PathBuf};
use std::fs;
use std::process::Command;
use tracing::info;
use regex::Regex;
use serde::Deserialize;

pub struct AssetProvisioner;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningStrategy {
    Auto,         // 默认：本地优先，本地缺失则走远程
    ForceRemote,  // 强制走官网拉取
    LocalOnly,    // 仅尝试从本地安装提取
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseInfo {
    pub version: String,
    pub execution_id: String,
}

#[derive(Debug, Clone)]
pub struct ProvisionedAssets {
    pub ls_core_path: PathBuf,
    pub cert_pem_path: PathBuf,
    pub ls_address: String,
    pub version: String,
}

impl AssetProvisioner {
    /// 包装方法，支持进度回调
    pub async fn ensure_assets_with_progress(
        strategy: ProvisioningStrategy, 
        on_progress: Box<dyn Fn(u32, &str) + Send + Sync>
    ) -> anyhow::Result<ProvisionedAssets> {
        on_progress(0, "正在初始化环境...");
        let bin_dir = crate::common::get_app_bin_dir();
        let data_dir = crate::common::get_app_data_dir();
        
        if !data_dir.exists() { fs::create_dir_all(&data_dir)?; }
        if !bin_dir.exists() { fs::create_dir_all(&bin_dir)?; }

        let target_ls = bin_dir.join("ls_core");
        let target_cert = bin_dir.join("cert.pem");
        let target_config_path = data_dir.join("ls_config.json");

        // 0. 读取当前已安装的完整配置 (优先从外部配置读取)
        let mut config = crate::common::get_runtime_config();
        let project_version = config.version.clone();
        
        let mut sync_performed = false;
        let assets_missing = !target_ls.exists() || !target_cert.exists();

        // 1. 本地提取逻辑 (本地安装优先)
        if strategy != ProvisioningStrategy::ForceRemote {
            if let Some(local_path) = Self::detect_local_antigravity_path() {
                let local_version = Self::detect_version_from_path(&local_path).unwrap_or_else(|| "unknown".to_string());
                
                if local_version != project_version || assets_missing || strategy == ProvisioningStrategy::LocalOnly {
                    on_progress(5, &format!("检测到本地版本 ({})，正在对齐资产...", local_version));
                    if let Some(src_ls) = Self::find_local_ls_bin(&local_path) {
                        fs::copy(&src_ls, &target_ls)?;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            fs::set_permissions(&target_ls, fs::Permissions::from_mode(0o755))?;
                        }
                    }
                    let src_cert = local_path.join("dist/languageServer/cert.pem");
                    if src_cert.exists() {
                        fs::copy(&src_cert, &target_cert)?;
                    }
                    let src_js = local_path.join("dist/extension.js");
                    if src_js.exists() {
                        if let Some(addr) = Self::extract_ls_address(&src_js) {
                            config.ls_address = addr;
                        }
                    }
                    config.version = local_version;
                    sync_performed = true;
                } else {
                    on_progress(10, "本地版本已对齐。");
                }
            }
        }

        // 2. 远程同步逻辑 (判定官网版本是否一致)
        if !sync_performed && strategy != ProvisioningStrategy::LocalOnly {
            if strategy == ProvisioningStrategy::ForceRemote || assets_missing || strategy == ProvisioningStrategy::Auto {
                on_progress(15, "正在获取云端版本信息...");
                if let Ok(latest) = Self::get_remote_latest_release().await {
                    if strategy == ProvisioningStrategy::ForceRemote || assets_missing || latest.version != project_version {
                        info!("Starting remote sync strategy (version: {})...", latest.version);
                        Self::provision_remote_release_with_progress(&bin_dir, &on_progress, latest.clone()).await?;
                        config.version = latest.version;
                        sync_performed = true;
                    } else {
                        on_progress(20, "云端版本已一致。");
                    }
                }
            }
        }

        // 3. 收尾 (同步持久化配置)
        if sync_performed || assets_missing {
            on_progress(95, "正在完成配置对齐...");
            if target_ls.exists() && target_cert.exists() {
                fs::write(&target_config_path, serde_json::to_string_pretty(&config)?)?;
            }
        }

        if !target_ls.exists() || !target_cert.exists() {
            return Err(anyhow::anyhow!("❌ 关键资产缺失。请手动将 ls_core 和 cert.pem 放入 bin 目录。"));
        }

        on_progress(100, "同步成功");
        Ok(ProvisionedAssets {
            ls_core_path: target_ls,
            cert_pem_path: target_cert,
            ls_address: config.ls_address,
            version: config.version,
        })
    }

    /// 保持向前兼容
    pub async fn ensure_assets(strategy: ProvisioningStrategy) -> anyhow::Result<ProvisionedAssets> {
        Self::ensure_assets_with_progress(strategy, Box::new(|_, _| {})).await
    }

    // ... (保留 detect_local_antigravity_path 等辅助方法)

    async fn get_remote_latest_release() -> anyhow::Result<ReleaseInfo> {
        let api_url = "https://antigravity-auto-updater-974169037036.us-central1.run.app/releases";
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
            
        let releases: Vec<ReleaseInfo> = client.get(api_url).send().await?.json().await?;
        releases.into_iter().next().ok_or_else(|| anyhow::anyhow!("API 未返回任何版本"))
    }

    async fn provision_remote_release_with_progress(
        dest_dir: &Path,
        on_progress: &(dyn Fn(u32, &str) + Send + Sync),
        latest: ReleaseInfo
    ) -> anyhow::Result<()> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()?;

        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;

        let (download_url, ext, bin_name) = if os == "linux" {
            on_progress(15, "正在从 Linux 仓库解析 DEB 地址...");
            let deb_url = Self::fetch_linux_deb_url(&latest.version, arch).await?;
            let bin_name = if arch == "aarch64" { "language_server_linux_arm" } else { "language_server_linux_x64" };
            (deb_url, "deb", bin_name)
        } else {
            let (platform_slug, ext, bin_name) = match (os, arch) {
                ("macos", "x86_64") => ("darwin-x64", "dmg", "language_server_macos_x64"),
                ("macos", "aarch64") => ("darwin-arm", "dmg", "language_server_macos_arm"),
                ("windows", "x86_64") => ("windows-x64", "exe", "language_server_windows_x64.exe"),
                ("windows", "aarch64") => ("windows-arm64", "exe", "language_server_windows_arm64.exe"),
                _ => return Err(anyhow::anyhow!("不支持的系统架构")),
            };
            let url = format!(
                "https://edgedl.me.gvt1.com/edgedl/release2/j0qc3/antigravity/stable/{}-{}/{}/Antigravity.{}",
                latest.version, latest.execution_id, platform_slug, ext
            );
            (url, ext, bin_name)
        };

        let work_dir = std::env::temp_dir().join(format!("antigravity-rust-fetch-{}", latest.execution_id));
        if work_dir.exists() { fs::remove_dir_all(&work_dir)?; }
        fs::create_dir_all(&work_dir)?;
        
        let package_path = work_dir.join(format!("package.{}", ext));
        
        on_progress(20, "启动流式下载...");
        let mut resp = client.get(&download_url).send().await?;
        if !resp.status().is_success() {
            return Err(anyhow::anyhow!("下载失败: HTTP {} for {}", resp.status(), download_url));
        }
        
        let total_size = resp.content_length().unwrap_or(0);
        let mut downloaded: u64 = 0;
        let mut file = std::fs::File::create(&package_path)?;

        use futures_util::StreamExt;
        let mut stream = resp.bytes_stream();
        
        while let Some(item) = stream.next().await {
            let chunk = item?;
            std::io::copy(&mut chunk.as_ref(), &mut file)?;
            downloaded += chunk.len() as u64;
            
            if total_size > 0 {
                let percent = 20 + ((downloaded as f64 / total_size as f64) * 60.0) as u32;
                if downloaded % (1024 * 1024 * 5) == 0 || downloaded == total_size {
                    on_progress(percent, &format!("已下载: {} / {} MB", downloaded / 1024 / 1024, total_size / 1024 / 1024));
                }
            }
        }
        drop(file); // 确保写入完成并关闭
        
        on_progress(85, "正在处理包内容...");
        match ext {
            "tar.gz" => {
                use flate2::read::GzDecoder;
                use tar::Archive;
                let tar_gz = fs::File::open(&package_path)?;
                let tar = GzDecoder::new(tar_gz);
                let mut archive = Archive::new(tar);
                archive.unpack(&work_dir)?;
                
                let resources_rel = "resources/app/extensions/antigravity";
                let mut ls_src = None;
                let mut cert_src = None;
                for entry in fs::read_dir(&work_dir)? {
                    let entry = entry?;
                    let path = entry.path();
                    if path.is_dir() {
                        let potential_ls = path.join(resources_rel).join("bin").join(bin_name);
                        let potential_cert = path.join(resources_rel).join("dist/languageServer/cert.pem");
                        if potential_ls.exists() && potential_cert.exists() {
                            ls_src = Some(potential_ls);
                            cert_src = Some(potential_cert);
                            break;
                        }
                    }
                }
                if let (Some(ls), Some(cert)) = (ls_src, cert_src) {
                    fs::copy(ls, dest_dir.join("ls_core"))?;
                    fs::copy(cert, dest_dir.join("cert.pem"))?;
                }
            }
            "deb" => {
                on_progress(82, "正在提取 DEB 包内容...");
                // 1. 提取 data.tar.xz (使用 ar 命令)
                let status = Command::new("ar")
                    .current_dir(&work_dir)
                    .args(["x", "package.deb", "data.tar.xz"])
                    .status()?;
                if !status.success() { return Err(anyhow::anyhow!("ar x 失败，请确保安装了 binutils")); }
                
                // 2. 提取内核和证书 (使用 tar 命令)
                // 路径参考：./usr/share/antigravity/resources/app/extensions/antigravity/bin/language_server_linux_x64
                let res_path = "./usr/share/antigravity/resources/app/extensions/antigravity";
                let ls_rel = format!("{}/bin/{}", res_path, bin_name);
                let cert_rel = format!("{}/dist/languageServer/cert.pem", res_path);
                
                let status = Command::new("tar")
                    .current_dir(&work_dir)
                    .args(["xf", "data.tar.xz", &ls_rel, &cert_rel])
                    .status()?;
                if !status.success() { return Err(anyhow::anyhow!("tar xf 提取核心文件失败")); }
                
                fs::copy(work_dir.join(ls_rel), dest_dir.join("ls_core"))?;
                fs::copy(work_dir.join(cert_rel), dest_dir.join("cert.pem"))?;
            }
            "dmg" => {
                on_progress(88, "正在挂载镜像文件...");
                let mount_pt = work_dir.join("mount");
                fs::create_dir_all(&mount_pt)?;
                let status = Command::new("hdiutil")
                    .args(["attach", package_path.to_str().unwrap(), "-mountpoint", mount_pt.to_str().unwrap(), "-quiet", "-nobrowse"])
                    .status()?;
                if !status.success() { return Err(anyhow::anyhow!("hdiutil attach 失败")); }
                
                on_progress(92, "正在提取内核文件...");
                let app_res = mount_pt.join("Antigravity.app/Contents/Resources/app/extensions/antigravity");
                fs::copy(app_res.join("bin").join(bin_name), dest_dir.join("ls_core"))?;
                fs::copy(app_res.join("dist/languageServer/cert.pem"), dest_dir.join("cert.pem"))?;
                Command::new("hdiutil").args(["detach", mount_pt.to_str().unwrap(), "-quiet"]).status()?;
            }
            "exe" => {
                on_progress(88, "正在提取资产 (7z)...");
                let status = Command::new("7z")
                    .args(["x", package_path.to_str().unwrap(), &format!("-o{}", work_dir.join("win").display()), "-y"])
                    .status()?;
                if !status.success() { return Err(anyhow::anyhow!("7z 提取失败")); }
                
                let mut found = false;
                for entry in walkdir::WalkDir::new(work_dir.join("win")) {
                    let entry = entry?;
                    if entry.file_name() == bin_name {
                        fs::copy(entry.path(), dest_dir.join("ls_core"))?;
                        found = true;
                    } else if entry.file_name() == "cert.pem" {
                        fs::copy(entry.path(), dest_dir.join("cert.pem"))?;
                    }
                }
                if !found { return Err(anyhow::anyhow!("在 EXE 中未找到核心资产")); }
            }
            _ => {}
        }
        
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dest_dir.join("ls_core"), fs::Permissions::from_mode(0o755))?;
        }
        
        fs::remove_dir_all(&work_dir)?;
        Ok(())
    }

    // 保留辅助方法...
    fn detect_local_antigravity_path() -> Option<PathBuf> {
        // 复用 ide.rs 中的增强探测逻辑
        let exe_path = crate::ide::get_antigravity_executable_path()?;
        
        #[cfg(target_os = "macos")]
        {
            // 如果是 .app 目录，资产在 Contents/Resources/app/extensions/antigravity
            let path = exe_path.join("Contents/Resources/app/extensions/antigravity");
            if path.exists() { return Some(path); }
            // 兼容直接指向 binary 的情况
            if let Some(parent) = exe_path.parent() {
                 let path = parent.parent().and_then(|p| p.join("Resources/app/extensions/antigravity").into());
                 if let Some(ref p) = path { if p.exists() { return Some(p.clone()); } }
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            // Windows/Linux: 资产在 resources/app/extensions/antigravity
            if let Some(parent) = exe_path.parent() {
                let path = parent.join("resources\\app\\extensions\\antigravity");
                if path.exists() { return Some(path); }
                // 兼容直接在 extensions 目录的情况
                let path_alt = parent.join("resources/app/extensions/antigravity");
                if path_alt.exists() { return Some(path_alt); }
            }
        }

        None
    }

    fn find_local_ls_bin(base_path: &Path) -> Option<PathBuf> {
        let bin_dir = base_path.join("bin");
        #[cfg(target_os = "macos")]
        {
            #[cfg(target_arch = "aarch64")]
            let bin_name = "language_server_macos_arm";
            #[cfg(target_arch = "x86_64")]
            let bin_name = "language_server_macos_x64";
            let p = bin_dir.join(bin_name);
            if p.exists() { return Some(p); }
        }
        #[cfg(target_os = "windows")]
        {
            let bin_name = "language_server_windows_x64.exe";
            let p = bin_dir.join(bin_name);
            if p.exists() { return Some(p); }
        }
        #[cfg(target_os = "linux")]
        {
             #[cfg(target_arch = "aarch64")]
            let bin_name = "language_server_linux_arm";
            #[cfg(target_arch = "x86_64")]
            let bin_name = "language_server_linux_x64";
            let p = bin_dir.join(bin_name);
            if p.exists() { return Some(p); }
        }
        None
    }

    fn extract_ls_address(js_path: &Path) -> Option<String> {
        let content = fs::read_to_string(js_path).ok()?;
        let re = Regex::new(r"([a-z0-9.-]+\.antigravity\.google:443)").ok()?;
        re.find(&content).map(|m| m.as_str().to_string())
    }

    fn detect_version_from_path(base_path: &Path) -> Option<String> {
        #[cfg(target_os = "macos")]
        {
            let mut current = base_path.to_path_buf();
            for _ in 0..6 {
                let info_plist = current.join("Info.plist");
                if info_plist.exists() {
                    if let Ok(content) = std::fs::read(&info_plist) {
                        if let Ok(plist) = plist::Value::from_reader(std::io::Cursor::new(content)) {
                            if let Some(dict) = plist.as_dictionary() {
                                if let Some(v) = dict.get("CFBundleShortVersionString").and_then(|v| v.as_string()) {
                                    return Some(v.to_string());
                                }
                            }
                        }
                    }
                }
                if !current.pop() { break; }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let mut current = base_path.to_path_buf();
            // 向上递归查找，最多 6 层，以找到 IDE 根目录的 product.json 或 package.json
            for _ in 0..6 {
                // 1. 优先尝试 product.json (ideVersion)
                let prod_json = current.join("product.json");
                if let Ok(content) = std::fs::read_to_string(prod_json) {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(v) = json.get("ideVersion").and_then(|v| v.as_str()) {
                            return Some(v.to_string());
                        }
                    }
                }

                // 2. 兜底 package.json (如果 product.json 不存在或没有 ideVersion)
                let pkg_json = current.join("package.json");
                if let Ok(content) = std::fs::read_to_string(pkg_json) {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                        // 如果是 extension 的 package.json (通常 version 较小)，我们继续往上找
                        // 对于 Antigravity，IDE 根目录的 version 是 1.107.0，extension 是 0.2.0
                        if let Some(v) = json.get("version").and_then(|v| v.as_str()) {
                            // 如果版本号不是 0.x.x，或者已经找不到更上层了，就返回
                            if !v.starts_with("0.") {
                                return Some(v.to_string());
                            }
                        }
                    }
                }

                if !current.pop() { break; }
            }
        }
        None
    }

    async fn provision_remote_assets(dest_dir: &Path) -> anyhow::Result<()> {
        if let Ok(latest) = Self::get_remote_latest_release().await {
            Self::provision_remote_release_with_progress(dest_dir, &|_, _| {}, latest).await
        } else {
            Err(anyhow::anyhow!("无法获取远程发布信息"))
        }
    }

    async fn fetch_linux_deb_url(version: &str, arch: &str) -> anyhow::Result<String> {
        let arch_slug = match arch {
            "x86_64" | "amd64" => "binary-amd64",
            "aarch64" | "arm64" => "binary-arm64",
            _ => return Err(anyhow::anyhow!("不支持的架构: {}", arch)),
        };
        let packages_url = format!(
            "https://us-central1-apt.pkg.dev/projects/antigravity-auto-updater-dev/dists/antigravity-debian/main/{}/Packages",
            arch_slug
        );
        
        info!("Fetching Packages from {}", packages_url);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()?;
            
        let content = client.get(&packages_url).send().await?.text().await?;
        
        let mut current_pkg = String::new();
        let mut current_ver = String::new();
        let mut current_file = String::new();
        
        for line in content.lines() {
            if line.starts_with("Package: ") {
                current_pkg = line.trim_start_matches("Package: ").trim().to_string();
            } else if line.starts_with("Version: ") {
                current_ver = line.trim_start_matches("Version: ").trim().to_string();
            } else if line.starts_with("Filename: ") {
                current_file = line.trim_start_matches("Filename: ").trim().to_string();
            } else if line.trim().is_empty() {
                if current_pkg == "antigravity" && (version == "latest" || current_ver.starts_with(version)) {
                    return Ok(format!("https://us-central1-apt.pkg.dev/projects/antigravity-auto-updater-dev/{}", current_file));
                }
                current_pkg.clear();
                current_ver.clear();
                current_file.clear();
            }
        }
        
        if current_pkg == "antigravity" && (version == "latest" || current_ver.starts_with(version)) {
             return Ok(format!("https://us-central1-apt.pkg.dev/projects/antigravity-auto-updater-dev/{}", current_file));
        }

        Err(anyhow::anyhow!("在 {} 中未找到版本 {}", packages_url, version))
    }
}
