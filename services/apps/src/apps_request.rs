use crate::apps_item::AppsItem;
use crate::apps_registry::hash;
use crate::apps_registry::{AppsError, AppsMgmtError};
use crate::apps_storage::{validate_package, AppsStorage};
use crate::apps_utils;
use crate::downloader::Downloader as Req;
use crate::downloader::{DownloadError, Downloader};
use crate::generated::common::*;
use crate::manifest::{Icons, Manifest};
use crate::shared_state::AppsSharedData;
use crate::update_manifest::UpdateManifest;
use common::traits::Shared;
use hex_slice::AsHex;
use log::{debug, error, info};
use md5::{Digest, Md5};
use std::collections::HashMap;
use std::convert::From;
use std::fs::{remove_dir_all, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;
use url::Url;
use version_compare::{CompOp, Version};
use zip_utils::verify_zip;

#[cfg(test)]
use crate::shared_state::APPS_SHARED_SHARED_DATA;

// A struct that removes a directory's content when it is dropped, unless
// it is marked explicitely to not do so.
struct DirRemover {
    path: PathBuf,
    remove: bool,
}

impl DirRemover {
    fn new(path: &PathBuf) -> Self {
        Self {
            path: path.clone(),
            remove: true,
        }
    }

    fn keep(&mut self) {
        self.remove = false;
    }
}

impl Drop for DirRemover {
    fn drop(&mut self) {
        if self.remove {
            let _ = remove_dir_all(&self.path);
        }
    }
}

struct SenderRemover {
    shared_data: Shared<AppsSharedData>,
    app_update_url: String,
}

impl Drop for SenderRemover {
    fn drop(&mut self) {
        let _ = self
            .shared_data
            .lock()
            .downloadings
            .remove(&self.app_update_url);
    }
}

// A simpler struct to restore apps status when error happens
// during download_and_apply
struct AppsStatusRestorer {
    need_restore: bool,
    shared_data: Shared<AppsSharedData>,
    is_update: bool,
    apps_item: AppsItem,
    manifest: Manifest,
}

impl AppsStatusRestorer {
    fn new(
        shared_data: Shared<AppsSharedData>,
        is_update: bool,
        apps_item: AppsItem,
        manifest: Manifest,
    ) -> Self {
        Self {
            need_restore: true,
            shared_data,
            is_update,
            apps_item,
            manifest,
        }
    }

    fn dont_restore(&mut self) {
        self.need_restore = false;
    }
}

impl Drop for AppsStatusRestorer {
    fn drop(&mut self) {
        if self.need_restore {
            self.shared_data.lock().registry.restore_apps_status(
                self.is_update,
                &self.apps_item,
                &self.manifest,
            );
        }
    }
}

#[derive(Default)]
pub struct AppsRequest {
    pub downloader: Downloader, // Keeping the downloader around for reuse.
    pub shared_data: Shared<AppsSharedData>,
}

const DOWNLOAD_TIMEOUT: u64 = 600; // 10 mins

impl AppsRequest {
    pub fn new(shared_data: Shared<AppsSharedData>) -> Self {
        Self {
            downloader: Downloader::default(),
            shared_data,
        }
    }

    // Create app_update_url to cancel_sender map in shared data
    // Cancel the downloading process is available by calling cancel_sender.send()
    // before the downloading finishes.
    fn get_available_zip(
        &mut self,
        app_update_url: &str,
        url: &str,
        path: &Path,
    ) -> Result<PathBuf, AppsMgmtError> {
        let zip_path = path.join("application.zip");
        debug!("Dowloading {} to {}", url, zip_path.as_path().display());

        let (result_recv, cancel_sender) =
            self.downloader.cancelable_download(url, zip_path.as_path());
        {
            let downloadings = &mut self.shared_data.lock().downloadings;
            set_or_update_canceller(downloadings, &app_update_url, cancel_sender);
        }
        let _sender_remover = SenderRemover {
            shared_data: self.shared_data.clone(),
            app_update_url: app_update_url.into(),
        };
        // Wait for 10 mins
        if let Ok(rec) = result_recv.recv_timeout(Duration::from_secs(DOWNLOAD_TIMEOUT)) {
            if let Err(err) = rec {
                error!(
                    "Downloading {} to {} failed: {:?}",
                    url,
                    zip_path.as_path().display(),
                    err
                );
                return Err(AppsMgmtError::PackageDownloadFailed(err));
            }
        } else {
            return Err(AppsMgmtError::PackageDownloadFailed(DownloadError::Other(
                "Timed Out".into(),
            )));
        }

        Ok(zip_path)
    }

    fn get_update_manifest(
        &mut self,
        url: &str,
        path: &Path,
    ) -> Result<(PathBuf, PathBuf), AppsMgmtError> {
        let base_path = path.join("downloading");
        let available_dir = AppsStorage::get_app_dir(&base_path, &hash(url).to_string())?;

        let update_manifest = available_dir.join("update.webmanifest");
        debug!("dowload {} to {}", url, available_dir.display());

        let (result_recv, cancel_sender) = self
            .downloader
            .cancelable_download(url, update_manifest.as_path());
        {
            let downloadings = &mut self.shared_data.lock().downloadings;
            set_or_update_canceller(downloadings, &url, cancel_sender);
        }
        let _sender_remover = SenderRemover {
            shared_data: self.shared_data.clone(),
            app_update_url: url.into(),
        };
        if let Ok(rec) = result_recv.recv_timeout(Duration::from_secs(DOWNLOAD_TIMEOUT)) {
            if let Err(err) = rec {
                error!(
                    "Downloading {} to {} failed: {:?}",
                    url,
                    update_manifest.as_path().display(),
                    err
                );
                if format!("{:?}", DownloadError::Http("304".into())) == format!("{:?}", err) {
                    return Err(AppsMgmtError::DownloadNotMoidified);
                }
                return Err(AppsMgmtError::ManifestDownloadFailed(err));
            }
        } else {
            return Err(AppsMgmtError::ManifestDownloadFailed(DownloadError::Other(
                "Timed Out".into(),
            )));
        }

        Ok((available_dir, update_manifest))
    }

    pub fn check_for_update(
        &mut self,
        webapp_path: &str,
        update_url: &str,
        is_auto_update: bool,
    ) -> Result<Option<AppsObject>, AppsServiceError> {
        debug!("check_for_update is_auto_update {}", is_auto_update);
        let app = self
            .shared_data
            .lock()
            .registry
            .get_by_update_url(&update_url)
            .ok_or(AppsServiceError::AppNotFound)?;

        let path = Path::new(&webapp_path);
        let (_, update_manifest) = match self.get_update_manifest(update_url, &path) {
            Ok((available_dir, update_manifest)) => (available_dir, update_manifest),
            Err(err) => {
                if format!("{:?}", AppsMgmtError::DownloadNotMoidified) == format!("{:?}", err) {
                    return Ok(None);
                }
                return Err(AppsServiceError::DownloadManifestFailed);
            }
        };

        if compare_version_hash(&app, update_manifest) {
            let mut app_obj = AppsObject::from(&app);
            app_obj.allowed_auto_download = is_auto_update;

            Ok(Some(app_obj))
        } else {
            Ok(None)
        }
    }

    pub fn download_and_apply(
        &mut self,
        webapp_path: &str,
        update_url: &str,
        is_update: bool,
    ) -> Result<AppsObject, AppsServiceError> {
        let path = Path::new(&webapp_path);
        let (available_dir, update_manifest) = match self.get_update_manifest(update_url, &path) {
            Ok((available_dir, update_manifest)) => (available_dir, update_manifest),
            Err(err) => {
                debug!("get_update_manifest err {:?}", err);
                if err.is_equal(AppsMgmtError::ManifestDownloadFailed(
                    DownloadError::Canceled,
                )) {
                    return Err(AppsServiceError::Canceled);
                }
                return Err(AppsServiceError::DownloadManifestFailed);
            }
        };
        let manifest = Manifest::read_from(update_manifest.clone())
            .map_err(|_| AppsServiceError::InvalidManifest)?;

        // Lock registry to do application registration, emit installing event
        // Release lock before downloading zip package. Let downloading happen
        // asyc among other threads
        let mut apps_item: AppsItem;
        let apps_item_restore: AppsItem;
        let mut restorer: AppsStatusRestorer;
        {
            let registry = &mut self.shared_data.lock().registry;
            // Need create appsItem object and add to db to reflect status
            apps_item = if is_update {
                let mut app = registry
                    .get_by_update_url(&update_url)
                    .ok_or(AppsServiceError::AppNotFound)?;
                apps_item_restore = app.clone();
                app.set_update_state(AppsUpdateState::Updating);

                app
            } else {
                let app_name = registry.get_unique_name(&manifest.get_name(), &update_url)?;
                let mut app = AppsItem::default(&app_name, registry.get_vhost_port());
                app.set_update_url(&update_url);

                apps_item_restore = app.clone();
                app.set_install_state(AppsInstallState::Installing);

                app
            };

            if !manifest.get_version().is_empty() {
                apps_item.set_version(&manifest.get_version());
            }

            restorer = AppsStatusRestorer::new(
                self.shared_data.clone(),
                is_update,
                apps_item_restore,
                manifest.clone(),
            );

            let _ = registry.save_app(is_update, &apps_item, &manifest)?;

            registry.broadcast_installing(is_update, AppsObject::from(&apps_item));
        }

        let mut available_dir_remover = DirRemover::new(&available_dir);

        let available_dir = available_dir.as_path();
        let update_manifest = update_manifest.as_path();
        info!("update_manifest: {}", update_manifest.display());

        let update_manifest = UpdateManifest::read_from(update_manifest)
            .map_err(|_| AppsServiceError::InvalidManifest)?;

        if update_manifest.package_path.is_empty() {
            error!("No package path.");
            return Err(AppsServiceError::InvalidManifest);
        }

        if AppsStorage::available_disk_space(&webapp_path) < update_manifest.packaged_size * 2 {
            error!("Do not have enough disk space.");
            return Err(AppsServiceError::DiskSpaceNotEnough);
        }

        let available_zip = match self.get_available_zip(
            update_url,
            &update_manifest.package_path,
            available_dir,
        ) {
            Ok(package) => package,
            Err(err) => {
                let mut error = Err(AppsServiceError::DownloadPackageFailed);
                debug!("get_available_zip error {}", err);
                if err.is_equal(AppsMgmtError::PackageDownloadFailed(
                    DownloadError::Canceled,
                )) {
                    error = Err(AppsServiceError::Canceled);
                }

                return error;
            }
        };

        info!("available_zip: {}", available_zip.display());
        // We can lock registry now, since no waiting job.
        let shared = &mut self.shared_data.lock();
        let config = shared.config.clone();
        let registry = &mut shared.registry;
        if let Err(err) = verify_zip(available_zip.as_path(), &config.cert_type, "inf") {
            error!("Verify zip error: {:?}", err);
            return Err(AppsServiceError::InvalidSignature);
        }

        let manifest = validate_package(available_zip.as_path())
            .map_err(|_| AppsServiceError::InvalidPackage)?;

        if !apps_utils::compare_manifests(&update_manifest, &manifest) {
            return Err(AppsServiceError::InvalidManifest);
        }

        // Here we can emit ready to apply download if we have sparate steps
        // asking user to apply download
        let _ =
            registry.apply_download(&mut apps_item, &available_dir, &manifest, &path, is_update)?;

        // Everything went fine, don't remove the available_dir directory.
        available_dir_remover.keep();
        // Everything went fine, keep current app status
        restorer.dont_restore();

        Ok(AppsObject::from(&apps_item))
    }

    pub fn download_and_apply_pwa(
        &mut self,
        webapp_path: &str,
        update_url: &str,
    ) -> Result<AppsObject, AppsServiceError> {
        let path = Path::new(&webapp_path);
        let download_dir =
            AppsStorage::get_app_dir(&path.join("downloading"), &hash(update_url).to_string())
                .map_err(|_| AppsServiceError::DownloadManifestFailed)?;
        let download_manifest = download_dir.join("manifest.webmanifest");
        let downloader = Req::default();

        let is_update = false;
        // 1. download manifest to cache dir.
        debug!("dowload {} to {}", update_url, download_manifest.display());
        if let Err(err) = downloader.download(update_url, download_manifest.as_path()) {
            error!(
                "Downloading {} to {} failed: {:?}",
                update_url,
                download_manifest.as_path().display(),
                err
            );
            return Err(AppsServiceError::DownloadManifestFailed);
        }

        let mut manifest = Manifest::read_from(&download_manifest)
            .map_err(|_| AppsServiceError::InvalidManifest)?;
        let app_name: String;
        let mut apps_item: AppsItem;
        // Lock registry to do application registration, emit installing event
        {
            let registry = &mut self.shared_data.lock().registry;
            app_name = registry.get_unique_name(&manifest.get_name(), &update_url)?;
            apps_item = AppsItem::default_pwa(&app_name, registry.get_vhost_port());
            apps_item.set_install_state(AppsInstallState::Installing);
            apps_item.set_update_url(&update_url);
            // We make no difference for update or new install at the moment.
            let _ = registry.save_app(false, &apps_item, &manifest);
            registry.broadcast_installing(is_update, AppsObject::from(&apps_item));
        }

        // 2-1. download icons to cached dir.
        let update_url_base =
            Url::parse(update_url).map_err(|_| AppsServiceError::InvalidManifest)?;
        let manifest_url_base = Url::parse(&apps_item.get_manifest_url())
            .map_err(|_| AppsServiceError::InvalidManifest)?;
        if let Some(icons_value) = manifest.get_icons() {
            let mut icons: Vec<Icons> =
                serde_json::from_value(icons_value).unwrap_or_else(|_| vec![]);
            for icon in &mut icons {
                let mut icon_src = icon.get_src();
                // If this is an absolute uri remove the leading / when building the download path
                // so we don't end up trying to use a /some/invalid/path/icon.png path.
                if icon_src.starts_with('/') {
                    let _ = icon_src.remove(0);
                }
                let icon_path = download_dir.join(&icon_src);
                let icon_dir = icon_path.parent().unwrap();
                let icon_url = update_url_base
                    .join(&icon.get_src())
                    .map_err(|_| AppsServiceError::InvalidManifest)?;
                let _ = AppsStorage::ensure_dir(&icon_dir);
                if let Err(err) = downloader.download(icon_url.as_str(), icon_path.as_path()) {
                    error!(
                        "Failed to download icon {} -> {:?} : {:?}",
                        icon_url, icon_path, err
                    );
                }
                let icon_cached_url = manifest_url_base
                    .join(&icon_src)
                    .map_err(|_| AppsServiceError::InvalidManifest)?;
                icon.set_src(icon_cached_url.as_str());
            }
            manifest.set_icons(serde_json::to_value(icons).unwrap());
        }

        // 2-2. update start url in cached manifest to absolute url
        let start_url = update_url_base
            .join(&manifest.get_start_url())
            .map_err(|_| AppsServiceError::InvalidStartUrl)?;
        manifest.set_start_url(start_url.as_str());
        Manifest::write_to(&download_manifest, &manifest)
            .map_err(|_| AppsServiceError::InvalidManifest)?;

        // 3. finish installation and reigster the pwa app.
        self.shared_data.lock().registry.apply_pwa(
            &mut apps_item,
            &download_dir.as_path(),
            &manifest,
            &path,
        )?;

        Ok(AppsObject::from(&apps_item))
    }

    pub fn broadcast_download_failed(
        &mut self,
        update_url: &str,
        reason: AppsServiceError,
        some_app: Option<AppsObject>,
    ) {
        let apps_object = if let Some(app) = some_app {
            app
        } else {
            let mut apps_item = AppsItem::default("", 0);
            apps_item.set_update_url(&update_url);
            AppsObject::from(&apps_item)
        };

        error!("broadcast event: app download failed {:?}", reason);
        self.shared_data
            .lock()
            .registry
            .event_broadcaster
            .broadcast_app_download_failed(DownloadFailedReason {
                apps_object,
                reason,
            });
    }
}

fn set_or_update_canceller(
    downloadings: &mut HashMap<String, Sender<()>>,
    url: &str,
    canceller: Sender<()>,
) {
    if let Some(sender) = downloadings.get_mut(url) {
        *sender = canceller;
    } else {
        downloadings.insert(url.into(), canceller);
    }
}

fn compute_manifest_hash<P: AsRef<Path>>(p: P) -> Result<String, AppsError> {
    let mut file = File::open(p)?;
    let mut content = Vec::new();
    let _ = file.read_to_end(&mut content);
    let mut hasher = Md5::new();
    hasher.update(content);
    let result = hasher.finalize();

    Ok(format!("{:02x}", result.plain_hex(false)))
}

fn compare_version_hash<P: AsRef<Path>>(app: &AppsItem, update_manifest: P) -> bool {
    let manifest = match UpdateManifest::read_from(&update_manifest) {
        Ok(manifest) => manifest,
        Err(_) => return false,
    };

    debug!("from update manifest{:#?}", manifest);
    debug!("hash from registry {}", app.get_manifest_hash());

    let mut is_update_available = false;
    if app.get_version().is_empty() || manifest.version.is_empty() {
        let hash_str = match compute_manifest_hash(&update_manifest) {
            Ok(hash) => hash,
            Err(_) => return false,
        };
        debug!("hash from update manifest{}", hash_str);
        if hash_str != app.get_manifest_hash() {
            is_update_available = true;
        }
    }

    if !app.get_version().is_empty() && !manifest.version.is_empty() {
        if let Some(manifest_version) = Version::from(&manifest.version) {
            if let Some(app_version) = Version::from(&app.get_version()) {
                is_update_available = manifest_version.compare(&app_version) == CompOp::Gt;
            }
        }
    }

    debug!("compare_version_hash update {}", is_update_available);
    is_update_available
}

#[test]
fn test_apply_pwa() {
    use crate::apps_registry::AppsRegistry;
    use crate::config;
    use config::Config;
    use std::env;

    let _ = env_logger::try_init();

    // Init apps from test-fixtures/webapps and verify in test-apps-dir-pwa.
    let current = env::current_dir().unwrap();
    let root_path = format!("{}/test-fixtures/webapps", current.display());
    let test_dir = format!("{}/test-fixtures/test-apps-dir-pwa", current.display());
    let test_path = Path::new(&test_dir);

    // This dir is created during the test.
    // Tring to remove it at the beginning to make the test at local easy.
    let _ = remove_dir_all(&test_path);

    println!("Register from: {}", &root_path);
    let config = Config {
        root_path,
        data_path: test_dir.clone(),
        uds_path: String::from("uds_path"),
        cert_type: String::from("test"),
        updater_socket: String::from("updater_socket"),
    };

    let vhost_port = 80;
    let mut registry = match AppsRegistry::initialize(&config, vhost_port) {
        Ok(v) => v,
        Err(err) => {
            panic!("err: {:?}", err);
        }
    };

    println!("registry.count(): {}", registry.count());
    assert_eq!(4, registry.count());

    // Test 1: apply from a local dir
    let src_manifest = current.join("test-fixtures/apps-from/pwa/manifest.webmanifest");
    let update_url = "https://pwa1.test/manifest.webmanifest";
    let download_dir = AppsStorage::get_app_dir(
        &test_path.join("downloading"),
        &format!("{}", hash(update_url)),
    )
    .unwrap();
    let download_manifest = download_dir.join("manifest.webmanifest");

    if let Err(err) = std::fs::create_dir_all(download_dir.as_path()) {
        println!("{:?}", err);
    }
    let _ = std::fs::copy(&src_manifest, &download_manifest).unwrap();
    let manifest = Manifest::read_from(&download_manifest).unwrap();
    let app_name = registry
        .get_unique_name(&manifest.get_name(), &update_url)
        .unwrap();
    if let Some(icons_value) = manifest.get_icons() {
        let icons: Vec<Icons> = serde_json::from_value(icons_value).unwrap_or_else(|_| vec![]);
        assert_eq!(4, icons.len());
    } else {
        panic!();
    }
    let mut apps_item = AppsItem::default(&app_name, vhost_port);
    apps_item.set_install_state(AppsInstallState::Installing);
    apps_item.set_update_url(&update_url);

    match registry.apply_pwa(
        &mut apps_item,
        &download_dir.as_path(),
        &manifest,
        &test_path,
    ) {
        Ok(_) => {
            assert_eq!(apps_item.get_name(), "helloworld");
            assert_eq!(apps_item.get_install_state(), AppsInstallState::Installed);
        }
        Err(err) => {
            println!("err: {:?}", err);
            panic!();
        }
    }

    let shared_data = &*APPS_SHARED_SHARED_DATA;

    // Test 2: download and apply from a remote url
    let app_url = "https://testpwa.github.io/manifest.webmanifest";
    let mut request = AppsRequest::new(shared_data.clone());
    match request.download_and_apply_pwa(&test_dir, app_url) {
        Ok(app) => {
            assert_eq!(app.name, "hellopwa");
            assert_eq!(app.removable, true);
        }
        Err(err) => {
            println!("err: {:?}", err);
            panic!();
        }
    }
    if let Some(app) = shared_data.lock().registry.get_by_update_url(app_url) {
        assert_eq!(app.get_name(), "hellopwa");

        let cached_dir = test_path.join("cached").join(app.get_name());
        let update_manifest = cached_dir.join("manifest.webmanifest");
        let manifest = Manifest::read_from(&update_manifest).unwrap();

        // start url should be absolute url of remote address
        assert_eq!(
            manifest.get_start_url(),
            "https://testpwa.github.io/index.html"
        );

        // icon url should be relative path of local cached address
        if let Some(icons_value) = manifest.get_icons() {
            let icons: Vec<Icons> = serde_json::from_value(icons_value).unwrap_or_else(|_| vec![]);
            assert_eq!(4, icons.len());
            let manifest_url_base = Url::parse(&app.get_manifest_url()).unwrap().join("/");
            for icon in icons {
                let icon_src = icon.get_src();
                let icon_url_base = Url::parse(&icon_src).unwrap().join("/");
                assert_eq!(icon_url_base, manifest_url_base);
            }
        } else {
            panic!();
        }
    } else {
        panic!();
    }
}

#[test]
fn test_compare_version_hash() {
    use std::env;

    let _ = env_logger::try_init();

    let current = env::current_dir().unwrap();
    let manifest_path = format!(
        "{}/test-fixtures/compare-version-hash/sample_update_manifest.webmanifest",
        current.display()
    );

    // version compare
    {
        let app_name = "test";
        let mut apps_item = AppsItem::default(&app_name, 443);
        apps_item.set_version("1.0.0");

        assert!(compare_version_hash(&apps_item, &manifest_path));
    }

    // version compare
    {
        let app_name = "test";
        let mut apps_item = AppsItem::default(&app_name, 443);
        apps_item.set_version("1.0.1");

        assert!(!compare_version_hash(&apps_item, &manifest_path));
    }

    // version compare
    {
        let app_name = "test";
        let mut apps_item = AppsItem::default(&app_name, 443);
        apps_item.set_version("1.0.2");

        assert!(!compare_version_hash(&apps_item, &manifest_path));
    }

    // hash
    {
        let app_name = "test";
        let mut apps_item = AppsItem::default(&app_name, 443);
        apps_item.set_manifest_hash("6bfc26d201fd94xxxxbdd31b63f4aa54");

        assert!(compare_version_hash(&apps_item, &manifest_path));
    }

    {
        let app_name = "test";
        let mut apps_item = AppsItem::default(&app_name, 443);
        apps_item.set_manifest_hash("6bfc26d201fd94a431bdd31b63f4aa54");

        assert!(!compare_version_hash(&apps_item, &manifest_path));
    }
}
