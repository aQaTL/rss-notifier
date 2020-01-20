use lettre::smtp::authentication::{Credentials as LettreCredentials, Mechanism};
use lettre::smtp::ConnectionReuseParameters;
use lettre::{ClientSecurity, ClientTlsParameters, SmtpClient, Transport};
use lettre_email::EmailBuilder;
use native_tls::Protocol;
use native_tls::TlsConnector;
use serde::Deserialize;
use std::io;

#[derive(Deserialize, Debug)]
struct Config {
	credentials: Credentials,
	recipients: Vec<String>,
}

#[derive(Deserialize, Debug)]
struct Credentials {
	username: String,
	password: String,
	domain: String,
}

impl Into<LettreCredentials> for Credentials {
	fn into(self) -> LettreCredentials {
		LettreCredentials::new(self.username, self.password)
	}
}

fn main() -> io::Result<()> {
	let cfg = toml::from_slice::<Config>(&std::fs::read("cfg.toml")?)?;

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
