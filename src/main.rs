use crate::rss_sites::{load_sites_from_file_async, Site};
use enum_from_impler::EnumFromImpler;
use futures::future::join_all;
use handlebars::Handlebars;
use lazy_static::lazy_static;
use lettre::smtp::authentication::Mechanism;
use lettre::smtp::ConnectionReuseParameters;
use lettre::{ClientSecurity, ClientTlsParameters, SmtpClient, Transport};
use lettre_email::EmailBuilder;
use log::*;
use native_tls::Protocol;
use native_tls::TlsConnector;
use rss::Channel;
use std::io;
use std::io::BufReader;
use std::process::exit;
use std::time::Duration;
use structopt::StructOpt;
use tokio::time::interval;
use url::Url;
use uuid::Uuid;

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
	},
}

fn main() {
	let opt = Opt::from_args();
	match opt {
		Opt::Run => run().unwrap(),
		Opt::List => (),
		Opt::Add { name, url } => add(name, url).unwrap(),
	}
}

const SITES_PATH: &str = "sites_v2.json";

fn run() -> io::Result<()> {
	if std::env::var("RUST_LOG").is_err() {
		std::env::set_var("RUST_LOG", "rss_notifier");
	}
	env_logger::builder().format_module_path(false).init();

	set_ctrlc_handler();

	tokio::runtime::Builder::new()
		.threaded_scheduler()
		.enable_io()
		.enable_time()
		.build()
		.unwrap()
		.block_on(start_email_task());

	Ok(())
}

#[allow(irrefutable_let_patterns)]
async fn start_email_task() {
	info!("Starting ticker");

	let mut sites = load_sites_from_file_async(SITES_PATH)
		.await
		.expect(&(String::from("Failed to load ") + SITES_PATH));

	let mut ticker = interval(Duration::from_secs(60 * 60));
	'ticker: while let _ = ticker.tick().await {
		info!("Checking sites...");

		match load_sites_from_file_async(SITES_PATH).await {
			Ok(new_sites) => sites = new_sites,
			Err(e) => warn!("Failed to load {}: {:?}", SITES_PATH, e),
		}

		let mut check_results = join_all(sites.iter_mut().map(check_site)).await;
		let results_with_new_items = check_results
			.iter()
			.filter(|(_, new_items)| !new_items.is_empty())
			.collect::<Vec<_>>();

		if results_with_new_items.is_empty() {
			info!("Nothing new");
			continue 'ticker;
		}

		let (subject, body) = generate_email(&results_with_new_items);

		info!("Sending email");
		if let Err(e) = send_email(subject, body) {
			error!("Failed to send email: {:?}", e);
			continue 'ticker;
		};
		info!("Email sent");

		for (site, new_items) in check_results.iter_mut() {
			if new_items.is_empty() {
				continue;
			}
			info!(
				"Site: {} has {} new items",
				site.name.as_deref().unwrap_or_default(),
				new_items.len()
			);
			site.last_accessed = chrono::offset::Utc::now();
		}

		if let Err(e) = rss_sites::save_sites_async(SITES_PATH, &sites).await {
			warn!("Failed to save {}: {:?}", SITES_PATH, e);
		}
	}
}

static EMAIL_TEMPLATE: &str = "email";

lazy_static! {
	static ref HANDLEBARS: Handlebars<'static> = {
		let mut handlebars = Handlebars::new();
		handlebars.set_strict_mode(true);
		if let Err(e) =
			handlebars.register_template_string(EMAIL_TEMPLATE, include_str!("email.hb"))
		{
			panic!("{:#?}", e)
		}
		handlebars
	};
}

fn generate_email(new_items: &[&(&mut Site, Vec<rss::Item>)]) -> (String, String) {
	let subject = match new_items.len() {
		1 => format!(
			"New updates from {}",
			new_items[0]
				.0
				.name
				.as_deref()
				.unwrap_or_else(|| new_items[0].0.url.as_str())
		),
		count => format!("New updates from {} feeds", count),
	};

	let body = match HANDLEBARS.render(EMAIL_TEMPLATE, &new_items) {
		Ok(v) => v,
		Err(e) => panic!("{:#?}", e),
	};

	(subject, body)
}

#[derive(Debug, EnumFromImpler)]
#[impl_from]
enum CheckSiteError {
	Request(reqwest::Error),
	Rss(rss::Error),
	Chrono(chrono::ParseError),
}

//TODO transform to closure once async closures are stabilized
async fn check_site(site: &mut Site) -> (&mut Site, Vec<rss::Item>) {
	match try_check_site(site as &Site).await {
		Ok(v) => (site, v),
		Err(e) => {
			error!(
				"Error checking: \"{}\" [{}]: {:?}",
				site.name.as_deref().unwrap_or_default(),
				site.url,
				e
			);
			(site, Vec::with_capacity(0))
		}
	}
}

async fn try_check_site(site: &Site) -> Result<Vec<rss::Item>, CheckSiteError> {
	let channel = fetch_rss(&site).await?;

	let mut new_items = Vec::new();

	for item in channel.items() {
		let pub_date = chrono::DateTime::parse_from_rfc2822(item.pub_date().unwrap_or_default())?;

		if pub_date > site.last_accessed {
			info!("New item: \"{}\"", item.title().unwrap_or_default());
			new_items.push(item.to_owned());
		}
	}

	Ok(new_items)
}

async fn fetch_rss(site: &Site) -> Result<Channel, CheckSiteError> {
	let rss_bytes = reqwest::get(site.url.clone()).await?.bytes().await?;
	Ok(Channel::read_from(BufReader::new(rss_bytes.as_ref()))?)
}

fn add(name: Option<String>, url: Url) -> io::Result<()> {
	let rss_sites = rss_sites::RssSites::new(SITES_PATH).unwrap();
	rss_sites.sites.lock().unwrap().push(rss_sites::Site {
		uuid: Uuid::new_v4(),
		name,
		url,
		last_accessed: chrono::MIN_DATE.and_hms(0, 0, 0),
	});
	rss_sites.save(SITES_PATH)?;
	Ok(())
}

fn set_ctrlc_handler() {
	ctrlc::set_handler(move || {
		warn!("Forced exit");
		exit(0);
	})
	.unwrap();
}

#[derive(Debug, EnumFromImpler)]
enum SendEmailError {
	#[impl_from]
	Io(std::io::Error),
	#[impl_from]
	TomlDeserialize(toml::de::Error),
	#[impl_from]
	Smtp(lettre::smtp::error::Error),
	#[impl_from]
	Lettre(lettre_email::error::Error),
	#[impl_from]
	Tls(native_tls::Error),
}

fn send_email(subject: impl Into<String>, body: impl Into<String>) -> Result<(), SendEmailError> {
	let cfg = toml::from_slice::<config::Config>(&std::fs::read("cfg.toml")?)?;

	let email = EmailBuilder::new()
		.to(cfg.recipients[0].as_str())
		.from(cfg.credentials.username.clone())
		.subject(subject)
		.html(body)
		.build()?;

	let mut tls_builder = TlsConnector::builder();
	tls_builder.min_protocol_version(Some(Protocol::Tlsv10));

	let tls_parameters =
		ClientTlsParameters::new(cfg.credentials.domain.clone(), tls_builder.build()?);

	pub const SUBMISSION_PORT: u16 = 465;

	let mut mailer = SmtpClient::new(
		(cfg.credentials.domain.as_str(), SUBMISSION_PORT),
		ClientSecurity::Wrapper(tls_parameters),
	)?
	.authentication_mechanism(Mechanism::Login)
	.credentials(cfg.credentials)
	.connection_reuse(ConnectionReuseParameters::ReuseUnlimited)
	.transport();

	mailer.send(email.into())?;

	mailer.close();
	Ok(())
}

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
	use log::*;
	use std::path::Path;
	use std::sync::{Arc, Mutex};

	use chrono::{DateTime, Utc};
	use enum_from_impler::EnumFromImpler;
	use notify::{RecommendedWatcher, RecursiveMode, Watcher};
	use serde::{Deserialize, Serialize};
	use tokio::task;
	use url::Url;

	#[derive(Serialize, Deserialize, Debug, Clone)]
	pub struct Site {
		pub uuid: uuid::Uuid,
		pub name: Option<String>,
		pub url: Url,
		pub last_accessed: DateTime<Utc>,
	}

	pub struct RssSites {
		pub sites: Arc<Mutex<Vec<Site>>>,
	}

	impl RssSites {
		pub fn new<P: AsRef<Path>>(path: P) -> notify::Result<RssSites> {
			debug!("initing RssSites");
			let rss_sites = RssSites {
				sites: Arc::new(Mutex::new(load_sites_from_file(&path).unwrap())),
			};
			let sites_c = rss_sites.sites.clone();
			let mut watcher: RecommendedWatcher = Watcher::new_immediate(move |res| {
				debug!("trying to unlock...");
				debug!("Current len: {}", sites_c.lock().unwrap().len());
				debug!("unlocked");
				match res {
					Ok(event) => println!("ok {:?}", event),
					Err(err) => println!("err {:?}", err),
				}
			})?;
			watcher.configure(notify::Config::PreciseEvents(true))?;
			watcher.watch(path, RecursiveMode::NonRecursive)?;

			Ok(rss_sites)
		}

		pub fn save<P: AsRef<Path>>(&self, path: P) -> std::io::Result<()> {
			let f = std::fs::File::create(path)?;
			serde_json::to_writer_pretty(f, &*self.sites.lock().unwrap()).unwrap();
			Ok(())
		}
	}

	fn load_sites_from_file<P: AsRef<Path>>(path: P) -> std::io::Result<Vec<Site>> {
		let data = match std::fs::read(&path) {
			Ok(v) => v,
			Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
				let f = std::fs::File::create(path)?;
				let sites = Vec::new();
				serde_json::to_writer_pretty(f, &sites)?;
				return Ok(sites);
			}
			Err(err) => return Err(err),
		};
		Ok(serde_json::from_slice(&data).unwrap())
	}

	#[derive(EnumFromImpler, Debug)]
	pub enum SitesAsyncIoError {
		#[impl_from]
		TokioIo(tokio::io::Error),
		#[impl_from]
		Serde(serde_json::Error),
	}

	pub async fn load_sites_from_file_async<P: AsRef<Path>>(
		path: P,
	) -> Result<Vec<Site>, SitesAsyncIoError> {
		use tokio::{fs, io};

		let data = match fs::read(&path).await {
			Ok(v) => v,
			Err(err) if err.kind() == io::ErrorKind::NotFound => {
				let path = path.as_ref().to_owned();

				task::spawn_blocking(move || {
					let f = std::fs::File::create(path)?;

					let sites: Vec<Site> = Vec::with_capacity(0);
					Result::<_, SitesAsyncIoError>::Ok(serde_json::to_writer_pretty(f, &sites)?)
				})
				.await
				.unwrap()?;

				return Ok(Vec::with_capacity(0));
			}
			Err(err) => return Err(err.into()),
		};

		Ok(task::spawn_blocking(move || serde_json::from_slice(&data))
			.await
			.unwrap()?)
	}

	pub async fn save_sites_async(
		path: &'static str,
		sites: &[Site],
	) -> Result<(), SitesAsyncIoError> {
		task::block_in_place(move || {
			let f = std::fs::File::create(path)?;
			Result::<_, SitesAsyncIoError>::Ok(serde_json::to_writer_pretty(f, sites)?)
		})
	}
}
