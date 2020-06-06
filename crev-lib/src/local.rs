use crate::{
    activity::ReviewActivity,
    id::{self, LockedId, PassphraseFn},
    util, Error, ProofStore, Result, TrustProofType,
};
use crev_common::{
    self,
    result::ResultExt as _,
    sanitize_name_for_fs, sanitize_url_for_fs,
    serde::{as_base64, from_base64},
};
use crev_data::{
    id::UnlockedId,
    proof::{self, trust::TrustLevel},
    Id, PublicId, Url,
};
use default::default;
use directories::ProjectDirs;
use insideout::InsideOut;
use resiter::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    ffi::OsString,
    fs,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
};

const CURRENT_USER_CONFIG_SERIALIZATION_VERSION: i64 = -1;

/// Random 32 bytes
fn generete_salt() -> Vec<u8> {
    crev_common::rand::random_vec(32)
}

/// Backfill the host salt
///
/// For people that have configs generated when
/// `host_salt` was not a thing - generate some
/// form of stable id
///
/// TODO: at some point this should no longer be neccessary
fn backfill_salt() -> Vec<u8> {
    crev_common::blake2b256sum(b"BACKFILLED_SUM")
}

fn is_none_or_empty(s: &Option<String>) -> bool {
    if let Some(s) = s {
        s.is_empty()
    } else {
        true
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UserConfig {
    pub version: i64,
    #[serde(rename = "current-id")]
    pub current_id: Option<Id>,
    #[serde(
        rename = "host-salt",
        serialize_with = "as_base64",
        deserialize_with = "from_base64",
        default = "backfill_salt"
    )]
    host_salt: Vec<u8>,

    #[serde(
        rename = "open-cmd",
        skip_serializing_if = "is_none_or_empty",
        default = "Option::default"
    )]
    pub open_cmd: Option<String>,
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            version: CURRENT_USER_CONFIG_SERIALIZATION_VERSION,
            current_id: None,
            host_salt: generete_salt(),
            open_cmd: None,
        }
    }
}

impl UserConfig {
    pub fn get_current_userid(&self) -> Result<&Id> {
        self.get_current_userid_opt()
            .ok_or_else(|| Error::CurrentIDNotSet)
    }
    pub fn get_current_userid_opt(&self) -> Option<&Id> {
        self.current_id.as_ref()
    }
}

/// Local config stored in `~/.config/crev`
///
/// This managed IDs, local proof repository, etc.
pub struct Local {
    root_path: PathBuf,
    cache_path: PathBuf,
    cur_url: Mutex<Option<Url>>,
    user_config: Mutex<Option<UserConfig>>,
}

impl Local {
    #[allow(clippy::new_ret_no_self)]
    fn new() -> Result<Self> {
        let proj_dir = ProjectDirs::from("", "", "crev")
            .expect("no valid home directory path could be retrieved from the operating system");
        let root_path = proj_dir.config_dir().into();
        let cache_path = proj_dir.cache_dir().into();
        Ok(Self {
            root_path,
            cache_path,
            cur_url: Mutex::new(None),
            user_config: Mutex::new(None),
        })
    }

    /// Where the config is stored
    pub fn get_root_path(&self) -> &Path {
        &self.root_path
    }

    /// Where temporary files are stored
    pub fn get_root_cache_dir(&self) -> &Path {
        &self.cache_path
    }

    /// Fails if it doesn't exist. See `auto_create_or_open()`
    pub fn auto_open() -> Result<Self> {
        let repo = Self::new()?;
        fs::create_dir_all(&repo.cache_remotes_path())?;
        if !repo.root_path.exists() || !repo.user_config_path().exists() {
            Err(Error::UserConfigNotInitialized)?;
        }

        *repo.user_config.lock().unwrap() = Some(repo.load_user_config()?);
        Ok(repo)
    }

    /// Fails if it already exists. See `auto_create_or_open()`
    pub fn auto_create() -> Result<Self> {
        let repo = Self::new()?;
        fs::create_dir_all(&repo.root_path)?;
        fs::create_dir_all(&repo.cache_remotes_path())?;

        let config_path = repo.user_config_path();
        if config_path.exists() {
            Err(Error::UserConfigAlreadyExists)?;
        }
        let config: UserConfig = default();
        repo.store_user_config(&config)?;
        *repo.user_config.lock().unwrap() = Some(config);
        Ok(repo)
    }

    /// Load the database from disk, or create one if needed.
    pub fn auto_create_or_open() -> Result<Self> {
        let repo = Self::new()?;
        let config_path = repo.user_config_path();
        if config_path.exists() {
            Self::auto_open()
        } else {
            Self::auto_create()
        }
    }

    /// Load config, and return Id configured as the current one
    pub fn read_current_id(&self) -> Result<crev_data::Id> {
        Ok(self.load_user_config()?.get_current_userid()?.to_owned())
    }

    /// Load config, and return Id configured as the current one
    pub fn read_current_id_opt(&self) -> Result<Option<crev_data::Id>> {
        Ok(self.load_user_config()?.get_current_userid_opt().cloned())
    }

    /// Calculate `for_id` that is used in a lot of operations
    ///
    /// * if `id_str` is given and parses correctly - convert to Id.
    /// * otherwise return current id
    pub fn get_for_id_from_str_opt(&self, id_str: Option<&str>) -> Result<Option<Id>> {
        id_str
            .map(|s| crev_data::id::Id::crevid_from_str(s).map_err(Error::from))
            .or_else(|| self.read_current_id_opt().inside_out())
            .inside_out()
    }

    pub fn get_for_id_from_str(&self, id_str: Option<&str>) -> Result<Id> {
        self.get_for_id_from_str_opt(id_str)?
            .ok_or_else(|| Error::IDNotSpecifiedAndCurrentIDNotSet)
    }

    /// Load config, update which Id is the current one, and save.
    pub fn save_current_id(&self, id: &Id) -> Result<()> {
        let path = self.id_path(id);
        if !path.exists() {
            Err(Error::IDFileNotFound)?;
        }

        *self.cur_url.lock().unwrap() = None;

        let mut config = self.load_user_config()?;
        config.current_id = Some(id.clone());
        // Change the old, backfilled `host_salt` the first time
        // the id is being switched
        if config.host_salt == backfill_salt() {
            config.host_salt = generete_salt();
        }
        self.store_user_config(&config)?;

        Ok(())
    }

    /// Same as get_root_path()
    pub fn user_dir_path(&self) -> PathBuf {
        self.root_path.clone()
    }

    /// Directory where yaml files for user identities are stored
    pub fn user_ids_path(&self) -> PathBuf {
        self.user_dir_path().join("ids")
    }

    /// Directory where git checkouts for user's own proof repos are stored
    ///
    /// This is separate from cache of other people's proofs
    pub fn user_proofs_path(&self) -> PathBuf {
        self.root_path.join("proofs")
    }

    /// Like `user_proofs_path` but checks if the dir exists
    pub fn user_proofs_path_opt(&self) -> Option<PathBuf> {
        let path = self.user_proofs_path();

        if path.exists() {
            Some(path)
        } else {
            None
        }
    }

    /// Path where this Id is stored as YAML
    fn id_path(&self, id: &Id) -> PathBuf {
        match id {
            Id::Crev { id } => self
                .user_ids_path()
                .join(format!("{}.yaml", crev_common::base64_encode(id))),
        }
    }

    /// Returns public Ids which belong to the current user.
    pub fn get_current_user_public_ids(&self) -> Result<Vec<PublicId>> {
        let ids_path = self.user_ids_path();
        let mut ids = vec![];
        for dir_entry in std::fs::read_dir(&ids_path)? {
            let locked_id = LockedId::read_from_yaml_file(&dir_entry?.path())?;
            ids.push(locked_id.to_public_id())
        }

        Ok(ids)
    }

    /// Path to crev's config file
    fn user_config_path(&self) -> PathBuf {
        self.user_dir_path().join("config.yaml")
    }

    /// Path where git checkouts of other people's proof repos are stored
    pub fn cache_remotes_path(&self) -> PathBuf {
        self.cache_path.join("remotes")
    }

    /// Cache where metadata about in-progress reviews (etc) is stored
    fn cache_activity_path(&self) -> PathBuf {
        self.cache_path.join("activity")
    }

    /// Yaml file path for in-progress review metadata
    fn cache_review_activity_path(
        &self,
        source: &str,
        name: &str,
        version: &semver::Version,
    ) -> PathBuf {
        self.cache_activity_path()
            .join("review")
            .join(sanitize_name_for_fs(source))
            .join(sanitize_name_for_fs(name))
            .join(sanitize_name_for_fs(&version.to_string()))
            .with_extension("yaml")
    }

    /// Save activity (in-progress review) to disk
    pub fn record_review_activity(
        &self,
        source: &str,
        name: &str,
        version: &semver::Version,
        activity: &ReviewActivity,
    ) -> Result<()> {
        let path = self.cache_review_activity_path(source, name, version);

        crev_common::save_to_yaml_file(&path, activity)
            .map_err(|e| Error::ReviewActivity(Box::new(e)))?;

        Ok(())
    }

    /// Load activity (in-progress review) from disk
    pub fn read_review_activity(
        &self,
        source: &str,
        name: &str,
        version: &semver::Version,
    ) -> Result<Option<ReviewActivity>> {
        let path = self.cache_review_activity_path(source, name, version);

        if path.exists() {
            Ok(Some(
                crev_common::read_from_yaml_file(&path)
                    .map_err(|e| Error::ReviewActivity(Box::new(e)))?,
            ))
        } else {
            Ok(None)
        }
    }

    /// Just returns the config, doesn't change anything
    pub fn load_user_config(&self) -> Result<UserConfig> {
        let path = self.user_config_path();

        let config_str = std::fs::read_to_string(&path)?;

        Ok(serde_yaml::from_str(&config_str).map_err(Error::UserConfigParse)?)
    }

    /// Writes the config to disk AND sets it as the current one
    pub fn store_user_config(&self, config: &UserConfig) -> Result<()> {
        let path = self.user_config_path();

        let config_str = serde_yaml::to_string(&config)?;

        util::store_str_to_file(&path, &config_str)?;

        *self.user_config.lock().unwrap() = Some(config.clone());
        Ok(())
    }

    /// Id in the config
    pub fn get_current_userid(&self) -> Result<Id> {
        self.get_current_userid_opt()?
            .ok_or_else(|| Error::CurrentIDNotSet)
    }

    /// Id in the config
    pub fn get_current_userid_opt(&self) -> Result<Option<Id>> {
        let config = self.load_user_config()?;
        Ok(config.current_id)
    }

    /// Just reads the yaml file, doesn't change any state
    pub fn read_locked_id(&self, id: &Id) -> Result<LockedId> {
        let path = self.id_path(&id);
        LockedId::read_from_yaml_file(&path)
    }

    /// Just reads the yaml file, doesn't change any state
    pub fn read_current_locked_id_opt(&self) -> Result<Option<LockedId>> {
        self.get_current_userid_opt()?
            .map(|current_id| self.read_locked_id(&current_id))
            .inside_out()
    }

    /// Just reads the yaml file, doesn't change any state
    pub fn read_current_locked_id(&self) -> Result<LockedId> {
        self.read_current_locked_id_opt()?
            .ok_or_else(|| Error::CurrentIDNotSet)
    }

    /// Just reads the yaml file and unlocks it, doesn't change any state
    pub fn read_current_unlocked_id_opt(
        &self,
        passphrase_callback: PassphraseFn<'_>,
    ) -> Result<Option<UnlockedId>> {
        self.get_current_userid_opt()?
            .map(|current_id| self.read_unlocked_id(&current_id, passphrase_callback))
            .inside_out()
    }

    /// Just reads the yaml file and unlocks it, doesn't change anything
    pub fn read_current_unlocked_id(
        &self,
        passphrase_callback: PassphraseFn<'_>,
    ) -> Result<UnlockedId> {
        self.read_current_unlocked_id_opt(passphrase_callback)?
            .ok_or_else(|| Error::CurrentIDNotSet)
    }

    /// Just reads the yaml file and unlocks it, doesn't change anything
    ///
    /// Asks for passphrase up to 5 times
    pub fn read_unlocked_id(
        &self,
        id: &Id,
        passphrase_callback: PassphraseFn<'_>,
    ) -> Result<UnlockedId> {
        let locked = self.read_locked_id(id)?;
        let mut i = 0;
        loop {
            let passphrase = passphrase_callback()?;
            match locked.to_unlocked(&passphrase) {
                Ok(o) => return Ok(o),
                Err(e) => {
                    eprintln!("Error: {}", e);
                    if i == 5 {
                        return Err(e);
                    }
                }
            }
            i += 1;
        }
    }

    /// Writes the Id to disk, doesn't change any state
    pub fn save_locked_id(&self, id: &id::LockedId) -> Result<()> {
        let path = self.id_path(&id.to_public_id().id);
        fs::create_dir_all(&path.parent().expect("Not /"))?;
        id.save_to(&path)
    }

    /// Git clone or init new remote Github crev-proof repo for the current user.
    ///
    /// Saves to `user_proofs_path`, so it's trusted as user's own proof repo.
    pub fn clone_proof_dir_from_git(
        &self,
        git_https_url: &str,
        use_https_push: bool,
    ) -> Result<()> {
        let push_url = if use_https_push {
            git_https_url.to_string()
        } else {
            match util::git::https_to_git_url(git_https_url) {
                Some(git_url) => git_url,
                None => {
                    eprintln!("warning: Could not deduce `ssh` push url. Call:");
                    eprintln!("warning: cargo crev git remote set-url --push origin <url>");
                    eprintln!("warning: manually, after id is generated.");
                    eprintln!("");
                    git_https_url.to_string()
                }
            }
        };

        let proof_dir =
            self.get_proofs_dir_path_for_url(&Url::new_git(git_https_url.to_owned()))?;

        if proof_dir.exists() {
            eprintln!(
                "Proof directory `{}` already exists. Will not clone.",
                proof_dir.display()
            );
            return Ok(());
        }

        self.ensure_proofs_root_exists()?;

        debug_assert!(git_https_url.starts_with("https://"));
        match util::git::clone(git_https_url, &proof_dir) {
            Ok(repo) => {
                eprintln!("{} cloned to {}", git_https_url, proof_dir.display());
                repo.remote_set_url("origin", &push_url)?;
            }
            Err(e) => {
                Err(Error::CouldNotCloneGitHttpsURL(Box::new((
                    git_https_url.to_string(),
                    e.to_string(),
                ))))?;
            }
        }

        Ok(())
    }

    /// Inits repo in `get_proofs_dir_path()`
    pub fn init_repo_readme_using_template(&self) -> Result<()> {
        const README_MARKER_V0: &str = "CREV_README_MARKER_V0";

        let proof_dir = self.get_proofs_dir_path()?;
        let path = proof_dir.join("README.md");
        if path.exists() {
            if let Some(line) = std::io::BufReader::new(std::fs::File::open(&path)?)
                .lines()
                .find(|line| {
                    if let Ok(ref line) = line {
                        line.trim() != ""
                    } else {
                        true
                    }
                })
            {
                if line?.contains(README_MARKER_V0) {
                    return Ok(());
                }
            }
        }

        std::fs::write(
            proof_dir.join("README.md"),
            &include_bytes!("../rc/doc/README.md")[..],
        )?;
        self.proof_dir_git_add_path(Path::new("README.md"))?;
        Ok(())
    }

    // Get path relative to `get_proofs_dir_path` to store the `proof`
    fn get_proof_rel_store_path(&self, proof: &proof::Proof, host_salt: &[u8]) -> PathBuf {
        crate::proof::rel_store_path(&proof, host_salt)
    }

    /// Proof repo URL associated with the current user Id
    fn get_cur_url(&self) -> Result<Option<Url>> {
        let url = self.cur_url.lock().unwrap().clone();
        Ok(if let Some(url) = url {
            Some(url)
        } else if let Some(locked_id) = self.read_current_locked_id_opt()? {
            *self.cur_url.lock().unwrap() = Some(locked_id.url.clone());
            Some(locked_id.url)
        } else {
            None
        })
    }

    /// Creates `user_proofs_path()`
    fn ensure_proofs_root_exists(&self) -> Result<()> {
        fs::create_dir_all(&self.user_proofs_path())?;
        Ok(())
    }

    /// Dir unique to this URL, inside `user_proofs_path()`
    pub fn get_proofs_dir_path_for_url(&self, url: &Url) -> Result<PathBuf> {
        let old_path = self.user_proofs_path().join(url.digest().to_string());
        let new_path = self.user_proofs_path().join(sanitize_url_for_fs(&url.url));

        if old_path.exists() {
            // we used to use less human-friendly path format; move directories
            // from old to new path
            // TODO: get rid of this in some point in the future
            std::fs::rename(&old_path, &new_path)?;
        }

        Ok(new_path)
    }

    /// Path where the `proofs` are stored under `git` repository.
    ///
    /// This function derives path from current user's URL
    pub fn get_proofs_dir_path_opt(&self) -> Result<Option<PathBuf>> {
        if let Some(url) = self.get_cur_url()? {
            Ok(Some(self.get_proofs_dir_path_for_url(&url)?))
        } else {
            Ok(None)
        }
    }

    /// This function derives path from current user's URL
    pub fn get_proofs_dir_path(&self) -> Result<PathBuf> {
        self.get_proofs_dir_path_opt()?
            .ok_or_else(|| Error::CurrentIDNotSet)
    }

    /// Creates new unsigned trust proof object, not edited
    ///
    /// Ensures the proof contains valid URLs for Ids where possible.
    ///
    /// Currently ignores previous proofs
    ///
    /// See `trust.sign_by(ownid)`
    pub fn build_trust_proof(
        &self,
        from_id: &PublicId,
        ids: Vec<Id>,
        proof_type: TrustProofType,
    ) -> Result<proof::trust::Trust> {
        if ids.is_empty() {
            Err(Error::NoIdsGiven)?;
        }

        let mut db = self.load_db()?;
        let mut public_ids = Vec::with_capacity(ids.len());

        for id in ids {
            let url = match db.lookup_url(&id) {
                crev_wot::UrlOfId::FromSelf(url) | crev_wot::UrlOfId::FromSelfVerified(url) => {
                    Some(url)
                }
                crev_wot::UrlOfId::FromOthers(maybe_url) => {
                    let maybe_url = maybe_url.url.clone();
                    self.fetch_url_into(&maybe_url, &mut db)?;
                    db.lookup_url(&id).from_self()
                }
                crev_wot::UrlOfId::None => None,
            };
            if let Some(url) = url {
                public_ids.push(PublicId::new(id, url.to_owned()));
            } else {
                public_ids.push(PublicId::new_id_only(id));
            }
        }

        Ok(from_id.create_trust_proof(
            &public_ids,
            match proof_type {
                TrustProofType::Trust => TrustLevel::Medium,
                TrustProofType::Distrust => TrustLevel::Distrust,
                TrustProofType::Untrust => TrustLevel::None,
            },
        )?)
    }

    /// Fetch other people's proof repostiory from a git URL, into the current database on disk
    pub fn fetch_url(&self, url: &str) -> Result<()> {
        let mut db = self.load_db()?;
        self.fetch_url_into(url, &mut db)
    }

    /// Fetch other people's proof repostiory from a git URL, directly into the given db (and disk too)
    pub fn fetch_url_into(&self, url: &str, mut db: &mut crev_wot::ProofDB) -> Result<()> {
        eprintln!("Fetching {}... ", url);
        let dir = self.fetch_remote_git(url)?;
        self.import_proof_dir_and_print_counts(&dir, url, &mut db)?;
        let mut db = crev_wot::ProofDB::new();
        let url = Url::new_git(url);
        let fetch_source = self.get_fetch_source_for_url(url.clone())?;
        db.import_from_iter(proofs_iter_for_path(dir).map(move |p| (p, fetch_source.clone())));
        eprintln!("Found proofs from:");
        for (id, count) in db.all_author_ids() {
            let tmp;
            let verified_state = match db.lookup_url(&id).from_self() {
                Some(verified_url) if verified_url == &url => "verified owner",
                Some(verified_url) => {
                    tmp = format!("copy from {}", verified_url.url);
                    &tmp
                }
                None => "copy from another repo",
            };
            println!("{:>8} {} ({})", count, id, verified_state);
        }
        Ok(())
    }

    /// Fetch proof repo URLs of trusted Ids
    pub fn fetch_trusted(
        &self,
        trust_params: crate::TrustDistanceParams,
        for_id: Option<&str>,
    ) -> Result<()> {
        let mut already_fetched_ids = HashSet::new();
        let mut already_fetched_urls = HashSet::new();
        let mut db = self.load_db()?;
        let for_id = self.get_for_id_from_str(for_id)?;

        loop {
            let trust_set = db.calculate_trust_set(&for_id, &trust_params);
            if !self.fetch_ids_not_fetched_yet(
                trust_set.trusted_ids().cloned(),
                &mut already_fetched_ids,
                &mut already_fetched_urls,
                &mut db,
            ) {
                break;
            }
        }
        Ok(())
    }

    /// Fetch (and discover) proof repo URLs of all known Ids
    fn fetch_all_ids_recursively(
        &self,
        mut already_fetched_urls: HashSet<String>,
        db: &mut crev_wot::ProofDB,
    ) -> Result<()> {
        let mut already_fetched_ids = HashSet::new();

        loop {
            if !self.fetch_ids_not_fetched_yet(
                db.all_known_ids().into_iter(),
                &mut already_fetched_ids,
                &mut already_fetched_urls,
                db,
            ) {
                break;
            }
        }
        Ok(())
    }

    /// True if something was fetched
    fn fetch_ids_not_fetched_yet(
        &self,
        ids: impl Iterator<Item = Id> + Send,
        already_fetched_ids: &mut HashSet<Id>,
        already_fetched_urls: &mut HashSet<String>,
        db: &mut crev_wot::ProofDB,
    ) -> bool {
        use std::sync::mpsc::channel;

        let mut something_was_fetched = false;
        let (tx, rx) = channel();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(8)
            .build()
            .unwrap();

        pool.scope(|scope| {
            for id in ids {
                let tx = tx.clone();

                if already_fetched_ids.contains(&id) {
                    continue;
                }

                if let Some(url) = db.lookup_url(&id).any_unverified() {
                    let url = &url.url;

                    if already_fetched_urls.contains(url) {
                        continue;
                    }
                    let url_clone = url.clone();
                    scope.spawn(move |_scope| {
                        tx.send((url_clone.clone(), self.fetch_remote_git(&url_clone)))
                            .expect("send to work");
                    });
                    already_fetched_urls.insert(url.clone());
                } else {
                    eprintln!("Error: No URL for {}", id);
                }
                already_fetched_ids.insert(id);
            }

            drop(tx);

            for (url, res) in rx.into_iter() {
                res.and_then(|dir| {
                    self.import_proof_dir_and_print_counts(&dir, &url, db)?;
                    something_was_fetched = true;
                    Ok(())
                })
                .unwrap_or_else(|e| {
                    eprintln!("Error: Failed to fetch {}: {}", url, e);
                });
            }
        });
        something_was_fetched
    }

    /// Per-url directory in `cache_remotes_path()`
    pub fn get_remote_git_cache_path(&self, url: &str) -> Result<PathBuf> {
        let digest = crev_common::blake2b256sum(url.as_bytes());
        let digest = crev_data::Digest::from_vec(digest);
        let old_path = self.cache_remotes_path().join(digest.to_string());
        let new_path = self.cache_remotes_path().join(sanitize_url_for_fs(url));

        if old_path.exists() {
            // we used to use less human-friendly path format; move directories
            // from old to new path
            // TODO: get rid of this in some point in the future
            std::fs::rename(&old_path, &new_path)?;
        }

        Ok(new_path)
    }

    /// `LocalUser` if it's current user's URL, or `crev_wot::FetchSource` for the URL.
    fn get_fetch_source_for_url(&self, url: Url) -> Result<crev_wot::FetchSource> {
        if let Some(own_url) = self.get_cur_url()? {
            if own_url == url {
                return Ok(crev_wot::FetchSource::LocalUser);
            }
        }
        Ok(crev_wot::FetchSource::Url(Arc::new(url)))
    }

    /// Fetch a git proof repository
    ///
    /// Returns url where it was cloned/fetched
    ///
    /// Adds the repo to the local proof repo cache.
    pub fn fetch_remote_git(&self, url: &str) -> Result<PathBuf> {
        let dir = self.get_remote_git_cache_path(url)?;

        if dir.exists() {
            let repo = git2::Repository::open(&dir)?;
            util::git::fetch_and_checkout_git_repo(&repo)?
        } else {
            util::git::clone(url, &dir)?;
        }

        Ok(dir)
    }

    /// Fetches and imports to the given db
    ///
    /// Same as `fetch_url_into`, but with more stats
    ///
    /// dir - where the proofs were downloaded to
    /// url - url from which it was fetched
    pub fn import_proof_dir_and_print_counts(
        &self,
        dir: &Path,
        url: &str,
        db: &mut crev_wot::ProofDB,
    ) -> Result<()> {
        let prev_pkg_review_count = db.unique_package_review_proof_count();
        let prev_trust_count = db.unique_trust_proof_count();

        let fetch_source = self.get_fetch_source_for_url(Url::new_git(url))?;
        db.import_from_iter(
            proofs_iter_for_path(dir.to_owned()).map(move |p| (p, fetch_source.clone())),
        );

        let new_pkg_review_count = db.unique_package_review_proof_count() - prev_pkg_review_count;
        let new_trust_count = db.unique_trust_proof_count() - prev_trust_count;

        let msg = match (new_trust_count > 0, new_pkg_review_count > 0) {
            (true, true) => format!(
                "new: {} trust, {} package reviews",
                new_trust_count, new_pkg_review_count
            ),
            (true, false) => format!("new: {} trust", new_trust_count,),
            (false, true) => format!("new: {} package reviews", new_pkg_review_count),
            (false, false) => "no updates".into(),
        };

        eprintln!("{:<60} {}", url, msg);
        Ok(())
    }

    /// Fetch and discover proof repos. Like `fetch_all_ids_recursively`,
    /// but adds `https://github.com/dpc/crev-proofs` and repos in cache that didn't belong to any Ids.
    pub fn fetch_all(&self) -> Result<()> {
        let mut fetched_urls = HashSet::new();
        let mut db = self.load_db()?;

        eprintln!("Fetching...");
        // Temporarily hardcode `dpc`'s proof-repo url
        let dpc_url = "https://github.com/dpc/crev-proofs";
        self.fetch_remote_git(dpc_url)
            .err_eprint_and_ignore()
            .map(|dir| {
                self.import_proof_dir_and_print_counts(&dir, dpc_url, &mut db)
                    .err_eprint_and_ignore();
            });
        fetched_urls.insert(dpc_url.to_owned());

        for entry in fs::read_dir(self.cache_remotes_path())? {
            let path = entry?.path();
            if !path.is_dir() {
                continue;
            }

            let url = match git2::Repository::open(&path) {
                Ok(repo) => Self::url_for_repo(&repo),
                Err(_) => continue,
            };

            match url {
                Ok(url) => {
                    self.get_fetch_source_for_url(Url::new_git(url))
                        .map(|fetch_source| {
                            db.import_from_iter(
                                proofs_iter_for_path(path.to_owned())
                                    .map(move |p| (p, fetch_source.clone())),
                            );
                        })
                        .err_eprint_and_ignore();
                }
                Err(e) => {
                    eprintln!("ERR: {} {}", path.display(), e);
                }
            }
        }

        self.fetch_all_ids_recursively(fetched_urls, &mut db)?;

        Ok(())
    }

    fn url_for_repo(repo: &git2::Repository) -> Result<String> {
        let remote = repo.find_remote("origin")?;
        let url = remote.url().ok_or_else(|| Error::OriginHasNoURL)?;
        Ok(url.to_string())
    }

    /// Run arbitrary git command in `get_proofs_dir_path()`
    pub fn run_git(&self, args: Vec<OsString>) -> Result<std::process::ExitStatus> {
        let proof_dir_path = self.get_proofs_dir_path()?;
        if !proof_dir_path.exists() {
            let id = self.read_current_locked_id()?;
            self.clone_proof_dir_from_git(&id.url.url, false)?;
        }

        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(proof_dir_path)
            .status()
            .expect("failed to execute git");

        Ok(status)
    }

    /// set `open_cmd` in the config
    pub fn store_config_open_cmd(&self, cmd: String) -> Result<()> {
        let mut config = self.load_user_config()?;
        config.open_cmd = Some(cmd);
        self.store_user_config(&config)?;
        Ok(())
    }

    /// Create a new proofdb, and populate it with local repo
    /// and cache content.
    pub fn load_db(&self) -> Result<crev_wot::ProofDB> {
        let mut db = crev_wot::ProofDB::new();
        db.import_from_iter(
            self.all_local_proofs()
                .map(move |p| (p, crev_wot::FetchSource::LocalUser)),
        );
        db.import_from_iter(proofs_iter_for_remotes_checkouts(
            self.cache_remotes_path(),
        )?);
        Ok(db)
    }

    /// The path must be inside `get_proofs_dir_path()`
    pub fn proof_dir_git_add_path(&self, rel_path: &Path) -> Result<()> {
        let proof_dir = self.get_proofs_dir_path()?;
        let repo = git2::Repository::open(&proof_dir)?;
        let mut index = repo.index()?;

        index.add_path(rel_path)?;
        index.write()?;
        Ok(())
    }

    /// Add a commit to user's proof repo
    pub fn proof_dir_commit(&self, commit_msg: &str) -> Result<()> {
        let proof_dir = self.get_proofs_dir_path()?;
        let repo = git2::Repository::open(&proof_dir)?;
        let mut index = repo.index()?;
        let tree_id = index.write_tree()?;
        let tree = repo.find_tree(tree_id)?;
        let head = repo.head()?.peel_to_commit()?;

        let signature = repo.signature()?;

        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            commit_msg,
            &tree,
            &[&head],
        )?;

        Ok(())
    }

    /// Prints `read_current_locked_id`
    pub fn show_current_id(&self) -> Result<()> {
        if let Some(id) = self.read_current_locked_id_opt()? {
            let id = id.to_public_id();
            println!("{} {}", id.id, id.url_display());
        }
        Ok(())
    }

    /// Generate a new identity in the local config.
    ///
    /// It's OK if the URL contains other identities. A new one will be added.
    ///
    /// The callback should provide a passphrase
    pub fn generate_id(
        &self,
        url: &str,
        use_https_push: bool,
        read_new_passphrase: impl FnOnce() -> std::io::Result<String>,
    ) -> Result<id::LockedId> {
        self.clone_proof_dir_from_git(&url, use_https_push)?;

        let unlocked_id = crev_data::id::UnlockedId::generate(crev_data::Url::new_git(url));
        let passphrase = read_new_passphrase()?;
        let locked_id = id::LockedId::from_unlocked_id(&unlocked_id, &passphrase)?;

        self.save_locked_id(&locked_id)?;
        self.save_current_id(unlocked_id.as_ref())?;
        self.init_repo_readme_using_template()?;
        Ok(locked_id)
    }

    /// Set given Id as the current one
    pub fn switch_id(&self, id_str: &str) -> Result<()> {
        let id: Id = Id::crevid_from_str(id_str)?;
        self.save_current_id(&id)?;

        Ok(())
    }

    /// See `read_locked_id`
    pub fn export_locked_id(&self, id_str: Option<String>) -> Result<String> {
        let id = if let Some(id_str) = id_str {
            let id = Id::crevid_from_str(&id_str)?;
            self.read_locked_id(&id)?
        } else {
            self.read_current_locked_id()?
        };

        Ok(id.to_string())
    }

    /// Parse `LockedId`'s YAML and write it to disk. See `save_locked_id`
    pub fn import_locked_id(&self, locked_id_serialized: &str) -> Result<PublicId> {
        let id = LockedId::from_str(locked_id_serialized)?;
        self.save_locked_id(&id)?;
        Ok(id.to_public_id())
    }

    /// All proofs from all local repos, regardless of current user's URL
    fn all_local_proofs(&self) -> impl Iterator<Item = proof::Proof> {
        match self.user_proofs_path_opt() {
            Some(path) => {
                Box::new(proofs_iter_for_path(path)) as Box<dyn Iterator<Item = proof::Proof>>
            }
            None => Box::new(vec![].into_iter()),
        }
    }
}

impl ProofStore for Local {
    fn insert(&self, proof: &proof::Proof) -> Result<()> {
        let rel_store_path = self.get_proof_rel_store_path(
            proof,
            &self
                .user_config
                .lock()
                .unwrap()
                .as_ref()
                .expect("User config loaded")
                .host_salt,
        );
        let path = self.get_proofs_dir_path()?.join(&rel_store_path);

        fs::create_dir_all(path.parent().expect("Not a root dir"))?;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .write(true)
            .open(path)?;

        file.write_all(proof.to_string().as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        drop(file);

        self.proof_dir_git_add_path(&rel_store_path)?;

        Ok(())
    }

    fn proofs_iter(&self) -> Result<Box<dyn Iterator<Item = proof::Proof>>> {
        Ok(Box::new(self.all_local_proofs()))
    }
}

/// Scan a directory of git checkouts. Assumes fetch source is the origin URL.
fn proofs_iter_for_remotes_checkouts(
    path: PathBuf,
) -> Result<impl Iterator<Item = (proof::Proof, crev_wot::FetchSource)>> {
    let dir = std::fs::read_dir(&path)?;
    Ok(dir
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            if let Ok(ty) = e.file_type() {
                if ty.is_dir() {
                    return Some(e.path());
                }
            }
            None
        })
        .filter_map(move |path| {
            let repo = git2::Repository::open(&path).ok()?;
            let origin = repo.find_remote("origin").ok()?;
            let fetch_source = crev_wot::FetchSource::Url(Arc::new(Url::new_git(origin.url()?)));
            Some(proofs_iter_for_path(path).map(move |p| (p, fetch_source.clone())))
        })
        .flat_map(|iter| iter))
}

/// Scan a git checkout or any subdirectory obtained from a known URL
fn proofs_iter_for_path(path: PathBuf) -> impl Iterator<Item = proof::Proof> {
    use std::ffi::OsStr;
    let file_iter = walkdir::WalkDir::new(&path)
        .into_iter()
        // skip dotfiles, .git dir
        .filter_entry(|e| e.file_name().to_str().map_or(true, |f| !f.starts_with('.')))
        .map_err(move |e| {
            Error::ErrorIteratingLocalProofStore(Box::new((path.to_owned(), e.to_string())))
        })
        .filter_map_ok(|entry| {
            let path = entry.path();
            if !path.is_file() {
                return None;
            }

            let osext_match: &OsStr = "crev".as_ref();
            match path.extension() {
                Some(osext) if osext == osext_match => Some(path.to_owned()),
                _ => None,
            }
        });

    fn parse_proofs(path: &Path) -> Result<Vec<proof::Proof>> {
        Ok(proof::Proof::parse_from(std::fs::File::open(&path)?)?)
    }

    file_iter
        .filter_map(|maybe_path| {
            maybe_path
                .map_err(|e| eprintln!("Failed scanning for proofs: {}", e))
                .ok()
        })
        .filter_map(|path| match parse_proofs(&path) {
            Ok(proofs) => Some(proofs.into_iter().filter_map(move |proof| {
                proof
                    .verify()
                    .map_err(|e| {
                        eprintln!(
                            "Verification failed for proof signed '{}' in {}: {} ",
                            proof.signature(),
                            path.display(),
                            e
                        )
                    })
                    .ok()
                    .map(|_| proof)
            })),
            Err(e) => {
                eprintln!("Error parsing proofs in {}: {}", path.display(), e);
                None
            }
        })
        .flat_map(|iter| iter)
}

#[test]
fn local_is_send_sync() {
    fn is<T: Send + Sync>() {}
    is::<Local>();
}
