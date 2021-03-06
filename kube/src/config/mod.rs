//! Kubernetes configuration objects from ~/.kube/config or in cluster environment
//!
//! Used to populate [`Config`] that is ultimately used to construct a [`Client`][crate::Client].
//!
//! Unless you have issues, prefer using `Config::infer` and pass it to a [`Client`][crate::Client].

mod exec;
mod file_config;
mod file_loader;
mod incluster_config;
mod utils;

use crate::{Error, Result};
pub use file_loader::KubeConfigOptions;
use file_loader::{ConfigLoader, Der};

use reqwest::header::{self, HeaderMap};

use std::time::Duration;

/// Configuration object detailing things like cluster_url, default namespace, root certificates, and timeouts
#[derive(Debug, Clone)]
pub struct Config {
    /// The configured cluster url
    pub cluster_url: reqwest::Url,
    /// The configured default namespace
    pub default_ns: String,
    /// The configured root certificate
    pub root_cert: Option<reqwest::Certificate>,
    /// Default headers to be used to communicate with the Kubernetes API
    pub headers: HeaderMap,
    /// Timeout for calls to the Kubernetes API.
    ///
    /// A value of `None` means no timeout
    pub timeout: std::time::Duration,
    /// Whether to accept invalid ceritifacts
    pub accept_invalid_certs: bool,
    /// The identity to use for communicating with the Kubernetes API
    /// along wit the password to decrypt it.
    ///
    /// This is stored in a raw buffer form so that Config can implement `Clone`
    /// (since [`reqwest::Identity`] does not currently implement `Clone`)
    pub(crate) identity: Option<(Vec<u8>, String)>,
}

impl Config {
    /// Construct a new config where only the `cluster_url` is set by the user.
    /// and everything else receives a default value.
    ///
    /// Most likely you want to use [`Config::infer`] to infer the config from
    /// the environment.
    pub fn new(cluster_url: reqwest::Url) -> Self {
        Self {
            cluster_url,
            default_ns: String::from("default"),
            root_cert: None,
            headers: HeaderMap::new(),
            timeout: DEFAULT_TIMEOUT,
            accept_invalid_certs: false,
            identity: None,
        }
    }

    /// Infer the config from the environment
    ///
    /// Done by attempting to load in-cluster environment variables first, and
    /// then if that fails, trying the local kubeconfig.
    ///
    /// Fails if inference from both sources fails
    pub async fn infer() -> Result<Self> {
        match Self::new_from_cluster_env() {
            Err(e1) => {
                trace!("No in-cluster config found: {}", e1);
                trace!("Falling back to local kubeconfig");
                let config = Self::new_from_kubeconfig(&KubeConfigOptions::default())
                    .await
                    .map_err(|e2| Error::Kubeconfig(format!("Failed to infer config: {}, {}", e1, e2)))?;

                Ok(config)
            }
            success => success,
        }
    }

    /// Read the config from the cluster's environment variables
    pub fn new_from_cluster_env() -> Result<Self> {
        let cluster_url = incluster_config::kube_server().ok_or_else(|| {
            Error::Kubeconfig(format!(
                "Unable to load in cluster config, {} and {} must be defined",
                incluster_config::SERVICE_HOSTENV,
                incluster_config::SERVICE_PORTENV
            ))
        })?;
        let cluster_url = reqwest::Url::parse(&cluster_url)
            .map_err(|e| Error::Kubeconfig(format!("Malformed url: {}", e)))?;

        let default_ns = incluster_config::load_default_ns()
            .map_err(|e| Error::Kubeconfig(format!("Unable to load incluster default namespace: {}", e)))?;

        let root_cert = incluster_config::load_cert()?;

        let token = incluster_config::load_token()
            .map_err(|e| Error::Kubeconfig(format!("Unable to load in cluster token: {}", e)))?;

        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Bearer {}", token))
                .map_err(|e| Error::Kubeconfig(format!("Invalid bearer token: {}", e)))?,
        );

        Ok(Self {
            cluster_url,
            default_ns,
            root_cert: Some(root_cert),
            headers,
            timeout: DEFAULT_TIMEOUT,
            accept_invalid_certs: false,
            identity: None,
        })
    }

    /// Returns a client builder based on the cluster information from the kubeconfig file.
    ///
    /// This allows to create your custom reqwest client for using with the cluster API.
    pub async fn new_from_kubeconfig(options: &KubeConfigOptions) -> Result<Self> {
        let loader = ConfigLoader::new_from_options(options).await?;
        let cluster_url = reqwest::Url::parse(&loader.cluster.server)
            .map_err(|e| Error::Kubeconfig(format!("Malformed url: {}", e)))?;

        let default_ns = loader
            .current_context
            .namespace
            .clone()
            .unwrap_or_else(|| String::from("default"));

        let token = match &loader.user.token {
            Some(token) => Some(token.clone()),
            None => {
                if let Some(exec) = &loader.user.exec {
                    let creds = exec::auth_exec(exec)?;
                    let status = creds.status.ok_or_else(|| {
                        Error::Kubeconfig("exec-plugin response did not contain a status".into())
                    })?;
                    status.token
                } else {
                    None
                }
            }
        };

        let mut accept_invalid_certs = false;
        let mut root_cert = None;
        let mut identity = None;

        if let Some(ca_bundle) = loader.ca_bundle()? {
            use std::convert::TryInto;
            for ca in ca_bundle {
                accept_invalid_certs = hacky_cert_lifetime_for_macos(&ca);
                root_cert = Some(ca.try_into()?);
            }
        }

        match loader.identity(IDENTITY_PASSWORD) {
            Ok(id) => identity = Some(id),
            Err(e) => {
                debug!("failed to load client identity from kubeconfig: {}", e);
                // last resort only if configs ask for it, and no client certs
                if let Some(true) = loader.cluster.insecure_skip_tls_verify {
                    accept_invalid_certs = true;
                }
            }
        }

        let mut headers = HeaderMap::new();

        match (
            utils::data_or_file(&token, &loader.user.token_file),
            (&loader.user.username, &loader.user.password),
        ) {
            (Ok(token), _) => {
                headers.insert(
                    header::AUTHORIZATION,
                    header::HeaderValue::from_str(&format!("Bearer {}", token))
                        .map_err(|e| Error::Kubeconfig(format!("Invalid bearer token: {}", e)))?,
                );
            }
            (_, (Some(u), Some(p))) => {
                let encoded = base64::encode(&format!("{}:{}", u, p));
                headers.insert(
                    header::AUTHORIZATION,
                    header::HeaderValue::from_str(&format!("Basic {}", encoded))
                        .map_err(|e| Error::Kubeconfig(format!("Invalid bearer token: {}", e)))?,
                );
            }
            _ => {}
        }

        Ok(Self {
            cluster_url,
            default_ns,
            root_cert,
            headers,
            timeout: DEFAULT_TIMEOUT,
            accept_invalid_certs,
            identity: identity.map(|i| (i, String::from(IDENTITY_PASSWORD))),
        })
    }

    // The identity functions are used to parse the stored identity buffer
    // into an `reqwest::Identity` type. We do this because `reqwest::Identity`
    // is not `Clone`. This allows us to store and clone the buffer and supply
    // the `Identity` in a just-in-time fashion.
    //
    // Note: this should be removed if/when reqwest implements [`Clone` for
    // `Identity`](https://github.com/seanmonstar/reqwest/issues/871)

    // feature = "rustls-tls" assumes the buffer is pem
    #[cfg(feature = "rustls-tls")]
    pub(crate) fn identity(&self) -> Option<reqwest::Identity> {
        let (identity, _identity_password) = self.identity.as_ref()?;
        Some(reqwest::Identity::from_pem(identity).expect("Identity buffer was not valid identity"))
    }

    // feature = "native-tls" assumes the buffer is pkcs12 der
    #[cfg(feature = "native-tls")]
    pub(crate) fn identity(&self) -> Option<reqwest::Identity> {
        let (identity, identity_password) = self.identity.as_ref()?;
        Some(
            reqwest::Identity::from_pkcs12_der(identity, identity_password)
                .expect("Identity buffer was not valid identity"),
        )
    }
}

// https://github.com/clux/kube-rs/issues/146#issuecomment-590924397
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(295);
const IDENTITY_PASSWORD: &str = " ";

// temporary catalina hack for openssl only
#[cfg(all(target_os = "macos", feature = "native-tls"))]
fn hacky_cert_lifetime_for_macos(ca: &Der) -> bool {
    use openssl::x509::X509;
    let ca = X509::from_der(&ca.0).expect("valid der is a der");
    ca.not_before()
        .diff(ca.not_after())
        .map(|d| d.days.abs() > 824)
        .unwrap_or(false)
}

#[cfg(any(not(target_os = "macos"), not(feature = "native-tls")))]
fn hacky_cert_lifetime_for_macos(_: &Der) -> bool {
    false
}

// Expose raw config structs
pub use file_config::{
    AuthInfo, AuthProviderConfig, Cluster, Context, ExecConfig, Kubeconfig, NamedCluster, NamedContext,
    NamedExtension, Preferences,
};
