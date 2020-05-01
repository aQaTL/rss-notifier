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
use std::io::{self, BufReader};
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
	Run {
		#[structopt(long)]
		no_webserver: bool,
	},
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
		Opt::Run { no_webserver } => run(!no_webserver).unwrap(),
		Opt::List => (),
		Opt::Add { name, url } => add(name, url).unwrap(),
	}
}

#[derive(Debug, EnumFromImpler)]
#[impl_from]
enum RunError {
	Io(std::io::Error),
	Webserver(webserver::WebserverError),
}

fn run(with_webserver: bool) -> Result<(), RunError> {
	if std::env::var("RUST_LOG").is_err() {
		std::env::set_var("RUST_LOG", "rss_notifier,actix_web");
	}
	env_logger::builder().format_module_path(false).init();

	set_ctrlc_handler();

	let mut rt = tokio::runtime::Builder::new()
		.threaded_scheduler()
		.enable_io()
		.enable_time()
		.build()
		.unwrap();

	let mut futures = vec![rt.spawn(start_email_task())];

	if with_webserver {
		let local_set = tokio::task::LocalSet::new();

		let system_fut = actix::System::run_in_tokio("pleaseworkimtired", &local_set);
		let webserver = webserver::webserver()?;

		let handle = local_set.spawn_local(async move {
			tokio::task::spawn_local(system_fut);
			webserver.await.unwrap();
		});
		futures.push(handle);
	}

	rt.block_on(join_all(futures.into_iter()));

	Ok(())
}

#[allow(irrefutable_let_patterns)]
async fn start_email_task() {
	info!("Starting ticker");

	let mut sites = load_sites_from_file_async()
		.await
		.expect("Failed to load sites");

	let mut ticker = interval(Duration::from_secs(60 * 60));
	'ticker: while let _ = ticker.tick().await {
		info!("Checking sites...");

		match load_sites_from_file_async().await {
			Ok(new_sites) => sites = new_sites,
			Err(e) => warn!("Failed to load sites: {:?}", e),
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

		if let Err(e) = rss_sites::save_sites_async(&sites).await {
			warn!("Failed to save sites: {:?}", e);
		}
	}
}

static EMAIL_TEMPLATE: &str = "email";
pub static INDEX_TEMPLATE: &str = "index";

lazy_static! {
	pub static ref HANDLEBARS: Handlebars<'static> = {
		let mut handlebars = Handlebars::new();
		handlebars.set_strict_mode(true);
		if let Err(e) =
			handlebars.register_template_string(EMAIL_TEMPLATE, include_str!("static/email.hb"))
		{
			panic!("{:#?}", e)
		}
		if let Err(e) =
			handlebars.register_template_string(INDEX_TEMPLATE, include_str!("static/index.html"))
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
	let rss_sites = rss_sites::RssSites::new().unwrap();
	rss_sites.sites.lock().unwrap().push(rss_sites::Site {
		uuid: Uuid::new_v4(),
		name,
		url,
		last_accessed: chrono::MIN_DATE.and_hms(0, 0, 0),
	});
	rss_sites.save(rss_sites::SITES_PATH)?;
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
#[impl_from]
enum SendEmailError {
	Io(std::io::Error),
	Config(config::ConfigLoadError),
	Smtp(lettre::smtp::error::Error),
	Lettre(lettre_email::error::Error),
	Tls(native_tls::Error),
}

fn send_email(subject: impl Into<String>, body: impl Into<String>) -> Result<(), SendEmailError> {
	let cfg = config::load_config()?;

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
	use enum_from_impler::EnumFromImpler;
	use lettre::smtp::authentication::Credentials as LettreCredentials;
	use serde::Deserialize;

	const CONFIG_FILE: &str = "cfg.toml";

	#[derive(Deserialize, Debug)]
	pub struct Config {
		pub credentials: Credentials,
		pub recipients: Vec<String>,
		pub webserver_address: std::net::SocketAddr,
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

	#[derive(Debug, EnumFromImpler)]
	#[impl_from]
	pub enum ConfigLoadError {
		Io(std::io::Error),
		Toml(toml::de::Error),
	}

	pub fn load_config() -> Result<Config, ConfigLoadError> {
		toml::from_slice::<Config>(&std::fs::read(CONFIG_FILE)?).map_err(From::from)
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

	pub const SITES_PATH: &str = "sites_v2.json";

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
		pub fn new() -> notify::Result<RssSites> {
			debug!("initing RssSites");
			let rss_sites = RssSites {
				sites: Arc::new(Mutex::new(load_sites_from_file().unwrap())),
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
			watcher.watch(SITES_PATH, RecursiveMode::NonRecursive)?;

			Ok(rss_sites)
		}

		pub fn save<P: AsRef<Path>>(&self, path: P) -> std::io::Result<()> {
			let f = std::fs::File::create(path)?;
			serde_json::to_writer_pretty(f, &*self.sites.lock().unwrap()).unwrap();
			Ok(())
		}
	}

	pub(crate) fn load_sites_from_file() -> std::io::Result<Vec<Site>> {
		let data = match std::fs::read(SITES_PATH) {
			Ok(v) => v,
			Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
				let f = std::fs::File::create(SITES_PATH)?;
				let sites = Vec::new();
				serde_json::to_writer_pretty(f, &sites)?;
				return Ok(sites);
			}
			Err(err) => return Err(err),
		};
		Ok(serde_json::from_slice(&data).unwrap())
	}

	#[derive(EnumFromImpler, Debug)]
	pub enum SitesIoError {
		#[impl_from]
		TokioIo(tokio::io::Error),
		#[impl_from]
		Serde(serde_json::Error),
	}

	pub async fn load_sites_from_file_async() -> Result<Vec<Site>, SitesIoError> {
		use tokio::{fs, io};

		let data = match fs::read(&SITES_PATH).await {
			Ok(v) => v,
			Err(err) if err.kind() == io::ErrorKind::NotFound => {
				task::spawn_blocking(move || {
					let f = std::fs::File::create(SITES_PATH)?;

					let sites: Vec<Site> = Vec::with_capacity(0);
					Result::<_, SitesIoError>::Ok(serde_json::to_writer_pretty(f, &sites)?)
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

	pub fn save_sites(sites: &[Site]) -> Result<(), SitesIoError> {
		let f = std::fs::File::create(SITES_PATH)?;
		Result::<_, SitesIoError>::Ok(serde_json::to_writer_pretty(f, sites)?)
	}

	pub async fn save_sites_async(sites: &[Site]) -> Result<(), SitesIoError> {
		task::block_in_place(move || save_sites(&sites))
	}
}

mod webserver {
	use crate::{config, rss_sites};
	use actix_web::dev::{HttpResponseBuilder, Server};
	use actix_web::middleware::Logger;
	use actix_web::web::{get, post, Form};
	use actix_web::{App, HttpResponse, HttpServer, Responder, ResponseError};
	use enum_from_impler::EnumFromImpler;
	use serde::Deserialize;
	use std::fmt::Display;
	use url::Url;

	#[derive(Debug, EnumFromImpler)]
	#[impl_from]
	pub enum WebserverError {
		Config(config::ConfigLoadError),
		Io(std::io::Error),
	}

	macro_rules! static_sites {
		($app:ident, $dir:tt, [$($file:tt),* $(,)*] $(,)*) => {{
			$app
			$(.route(
				$file,
				actix_web::web::get().to(||
					actix_web::HttpResponse::Ok()
						.set_header(actix_web::http::header::CONTENT_TYPE, mime_guess::from_path($file).first_or_octet_stream())
						.body(include_str!(concat!($dir, $file)))
				)
			))*
		}};
	}

	pub fn webserver() -> Result<Server, WebserverError> {
		let cfg = config::load_config()?;

		let server = HttpServer::new(move || {
			let app = App::new()
				.wrap(Logger::new(r#" %a "%r" %s %T"#))
				.route("/add_site", post().to(add_site))
				.route("/", get().to(index));
			static_sites!(app, "static/", ["98.css"])
		})
		.bind(cfg.webserver_address)?;
		Ok(server.run())
	}

	async fn index() -> impl Responder {
		let render = crate::HANDLEBARS
			.render(crate::INDEX_TEMPLATE, &())
			.unwrap();
		HttpResponse::Ok().content_type("text/html").body(render)
	}

	#[derive(EnumFromImpler, Debug)]
	#[impl_from]
	enum AddSiteError {
		Io(std::io::Error),
		RssSitesDb(rss_sites::SitesIoError),
	}

	impl ResponseError for AddSiteError {
		fn error_response(&self) -> HttpResponse {
			log::error!("Error: {:?}", self);
			HttpResponseBuilder::new(self.status_code()).body(format!("{}", self))
		}
	}

	impl Display for AddSiteError {
		fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
			write!(f, "<h1>internal server error</h1>")
		}
	}

	#[derive(Deserialize)]
	struct NewSite {
		name: Option<String>,
		url: Url,
	}

	async fn add_site(Form(new_site): Form<NewSite>) -> Result<impl Responder, AddSiteError> {
		let mut sites = rss_sites::load_sites_from_file()?;
		sites.push(rss_sites::Site {
			uuid: uuid::Uuid::new_v4(),
			name: new_site.name,
			url: new_site.url,
			last_accessed: chrono::Utc::now(),
		});
		rss_sites::save_sites(&sites)?;
		Ok(HttpResponse::Created().body(""))
	}
}
