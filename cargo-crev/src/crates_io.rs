use crate::{deps::DownloadsStats, prelude::*};
use failure::ResultExt;
use serde::{de::DeserializeOwned, Serialize};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

pub struct Client {
    client: crates_io_api::SyncClient,
    cache_dir: PathBuf,
}

fn is_fresh(path: &Path) -> Result<bool> {
    let metadata = fs::metadata(path)?;
    let created = metadata.created().or_else(|_e| metadata.modified())?;
    let now = std::time::SystemTime::now();
    Ok(((now - Duration::from_secs(60 * 60 * 72)) < created) && (created < now))
}

trait Cacheable: Sized {
    fn get_cache_path(base: &Path, name: &str, version: &str) -> PathBuf;
    fn fetch(client: &crates_io_api::SyncClient, crate_: &str, _version: &str) -> Result<Self>;
}

impl Cacheable for crates_io_api::CrateResponse {
    fn get_cache_path(base: &Path, name: &str, _version: &str) -> PathBuf {
        base.join("crate").join(format!("{}.json", name))
    }
    fn fetch(client: &crates_io_api::SyncClient, crate_: &str, _version: &str) -> Result<Self> {
        Ok(client.get_crate(crate_).compat()?)
    }
}

impl Cacheable for crates_io_api::Owners {
    fn get_cache_path(base: &Path, name: &str, _version: &str) -> PathBuf {
        base.join("owners").join(format!("{}.json", name))
    }
    fn fetch(client: &crates_io_api::SyncClient, crate_: &str, _version: &str) -> Result<Self> {
        Ok(crates_io_api::Owners {
            users: client.crate_owners(crate_).compat()?,
        })
    }
}

fn get_downloads_stats(resp: &crates_io_api::CrateResponse, version: &Version) -> DownloadsStats {
    DownloadsStats {
        version: resp
            .versions
            .iter()
            .find(|v| v.num == version.to_string())
            .map(|v| v.downloads)
            .unwrap_or(0),
        total: resp.crate_data.downloads,
        recent: resp.crate_data.recent_downloads.unwrap_or(0),
    }
}

impl Client {
    pub fn new(local: &crev_lib::Local) -> Result<Self> {
        let cache_dir = local.get_root_cache_dir().join("crates_io");
        fs::create_dir_all(&cache_dir)?;
        Ok(Self {
            client: crates_io_api::SyncClient::new(
                "cargo-crev (https://github.com/crev-dev/cargo-crev)",
                std::time::Duration::from_secs(1)
            ).expect("Invalid user agent"),
            cache_dir,
        })
    }

    fn load_cache(&self, path: &Path) -> Result<String> {
        let mut file = std::fs::File::open(path)?;
        let mut content = String::new();
        file.read_to_string(&mut content)?;

        Ok(content)
    }

    fn get_from_cache<T: Cacheable + DeserializeOwned>(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Option<(T, bool)>> {
        let path = T::get_cache_path(&self.cache_dir, name, version);
        if path.exists() {
            let content = self.load_cache(&path)?;
            let v = serde_json::from_str::<T>(&content)?;
            Ok(Some((v, is_fresh(&path)?)))
        } else {
            Ok(None)
        }
    }

    fn store_in_cache<T: Cacheable + Serialize>(&self, path: &Path, resp: &T) -> Result<()> {
        crev_common::store_to_file_with(&path, |file| serde_json::to_writer(file, &resp))??;
        Ok(())
    }

    fn fetch<T: Cacheable + Serialize>(&self, crate_: &str, version: &str) -> Result<T> {
        let resp = T::fetch(&self.client, crate_, version)?;
        self.store_in_cache(&T::get_cache_path(&self.cache_dir, crate_, version), &resp)?;
        Ok(resp)
    }

    fn get<T: Cacheable + DeserializeOwned + Serialize>(
        &self,
        crate_: &str,
        version: &str,
    ) -> Result<T> {
        let cached: Option<(T, bool)> = self.get_from_cache(crate_, version)?;

        match cached {
            Some((resp, true)) => Ok(resp),
            Some((resp, false)) => match self.fetch(crate_, version) {
                Ok(new_resp) => Ok(new_resp),
                Err(_e) => Ok(resp),
            },
            None => self.fetch(crate_, version),
        }
    }

    pub fn get_downloads_count(&self, crate_: &str, version: &Version) -> Result<DownloadsStats> {
        Ok(get_downloads_stats(
            &self.get::<crates_io_api::CrateResponse>(crate_, &version.to_string())?,
            version,
        ))
    }

    pub fn get_owners(&self, crate_: &str) -> Result<Vec<String>> {
        let owners = self.get::<crates_io_api::Owners>(crate_, "")?;
        Ok(owners.users.into_iter().map(|u| u.login).collect())
    }
}
