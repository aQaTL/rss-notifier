use lettre::smtp::authentication::Mechanism;
use lettre::smtp::ConnectionReuseParameters;
use lettre::{ClientSecurity, ClientTlsParameters, SmtpClient, Transport};
use lettre_email::EmailBuilder;
use native_tls::Protocol;
use native_tls::TlsConnector;
use rss_sites::RssSites;
use std::io;
use std::sync::Arc;
use structopt::StructOpt;

mod config {
	use lettre::smtp::authentication::Credentials as LettreCredentials;
	use serde::Deserialize;

	#[derive(Deserialize, Debug)]
	pub struct Config {
		pub credentials: Credentials,
		pub recipients: Vec<String>,
	}

	#[derive(Deserialize, Debug)]
	pub struct Credentials {
		pub username: String,
		pub password: String,
		pub domain: String,
	}

	impl Into<LettreCredentials> for Credentials {
		fn into(self) -> LettreCredentials {
			LettreCredentials::new(self.username, self.password)
		}
	}
}

mod rss_sites {
	use chrono::{DateTime, Utc};
	use notify::{RecommendedWatcher, RecursiveMode, Watcher};
	use serde::{Deserialize, Serialize};
	use std::io;
	use std::path::Path;
	use std::sync::{Arc, Mutex};

	#[derive(Serialize, Deserialize)]
	pub struct Site {
		pub uuid: uuid::Uuid,
		pub name: Option<String>,
		pub url: String,
		pub last_accessed: DateTime<Utc>,
	}

	pub struct RssSites {
		pub sites: Arc<Mutex<Vec<Site>>>,
	}

	impl RssSites {
		pub fn new<P: AsRef<Path>>(path: P) -> notify::Result<RssSites> {
			let rss_sites = RssSites {
				sites: Arc::new(Mutex::new(load_sites_from_file(&path).unwrap())),
			};
			let sites_c = rss_sites.sites.clone();
			let mut watcher: RecommendedWatcher = Watcher::new_immediate(move |res| {
				println!("Current len: {}", sites_c.lock().unwrap().len());
				match res {
					Ok(event) => println!("ok {:?}", event),
					Err(err) => println!("err {:?}", err),
				}
			})?;
			watcher.configure(notify::Config::PreciseEvents(true))?;
			watcher.watch(&path, RecursiveMode::NonRecursive)?;

			Ok(rss_sites)
		}
	}

	fn load_sites_from_file<P: AsRef<Path>>(path: P) -> io::Result<Vec<Site>> {
		let data = match std::fs::read(&path) {
			Ok(v) => v,
			Err(err) if err.kind() == io::ErrorKind::NotFound => {
				let f = std::fs::File::create(path)?;
				let sites = Vec::new();
				serde_json::to_writer_pretty(f, &sites)?;
				return Ok(sites);
			}
			Err(err) => return Err(err),
		};
		Ok(serde_json::from_slice(&data).unwrap())
	}
}

#[derive(StructOpt, Debug)]
#[structopt(name = "rss-notifier")]
enum Opt {
	List,
	Run,
	Add {
		#[structopt(short, long)]
		name: Option<String>,
		#[structopt(parse(try_from_str))]
		url: Url,

	}
}

#[derive(Debug)]
struct Url(String);

#[derive(Debug)]
enum UrlParseErr {
	Generic,
}

impl std::string::ToString for UrlParseErr {
	fn to_string(&self) -> String {
		"Generic Error".to_string()
	}
}

impl std::str::FromStr for Url {
	type Err = UrlParseErr;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		Ok(Self(s.to_owned()))
	}
}

fn main() -> io::Result<()> {
	let opt = Opt::from_args();
	println!("{:?}", opt);

	let path = "sites_v2.json";
	let rss_sites = rss_sites::RssSites::new(path).unwrap();
	set_ctrlc_handler(path, &rss_sites);

	{
		let mut buf = [0u8; 1];
		std::io::Read::read(&mut std::io::stdin(), &mut buf[..]).map(|_| ())
	}
}

fn set_ctrlc_handler(path: &'static str, rss_sites: &RssSites) {
	let sites = Arc::clone(&rss_sites.sites);
	ctrlc::set_handler(move || {
		println!("exit");
		let sites = match sites.lock() {
			Ok(v) => v,
			Err(err) => err.into_inner(),
		};
		let file = std::fs::File::create(&path).unwrap();
		serde_json::to_writer_pretty(file, &*sites).unwrap();
	})
	.unwrap();
}

fn send_email() -> io::Result<()> {
	let cfg = toml::from_slice::<config::Config>(&std::fs::read("cfg.toml")?)?;

	let email = EmailBuilder::new()
		.to(cfg.recipients[0].as_str())
		.from(format!("{}", cfg.credentials.username))
		.subject("Pierwsza koperta")
		.html(format!(
			r#"<h1>Hello</h1>\
        <h2>Email test</h2>\
        Your rss feed isn't ready yet."#
		))
		.build()
		.unwrap();

	let mut tls_builder = TlsConnector::builder();
	tls_builder.min_protocol_version(Some(Protocol::Tlsv10));

	let tls_parameters =
		ClientTlsParameters::new(cfg.credentials.domain.clone(), tls_builder.build().unwrap());

	pub const SUBMISSION_PORT: u16 = 465;

	let mut mailer = SmtpClient::new(
		(cfg.credentials.domain.as_str(), SUBMISSION_PORT),
		ClientSecurity::Wrapper(tls_parameters),
	)
	.expect("Failed to create transport")
	.authentication_mechanism(Mechanism::Login)
	.credentials(cfg.credentials)
	.connection_reuse(ConnectionReuseParameters::ReuseUnlimited)
	.transport();

	println!("{:?}", mailer.send(email.into()));

	mailer.close();
	Ok(())
}
