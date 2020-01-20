use std::fs::File;
use std::io::{self, BufReader, BufWriter, ErrorKind};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub type Sites = Vec<Site>;

#[derive(Debug, Serialize, Deserialize)]
pub struct Site {
	pub url: String,
	pub last_checked: DateTime<Utc>,
}

static SITES_FILENAME: &'static str = "sites.json";

pub fn load() -> io::Result<Sites> {
	let sites_file = match File::open(SITES_FILENAME) {
		Ok(f) => f,
		Err(e) => {
			return if e.kind() == ErrorKind::NotFound {
				Ok(Vec::new())
			} else {
				Err(e)
			};
		}
	};
	let sites = serde_json::from_reader(BufReader::new(sites_file))?;
	Ok(sites)
}

pub fn save(sites: &Sites) -> io::Result<()> {
	let sites_file = File::create(SITES_FILENAME)?;
	match serde_json::to_writer_pretty(BufWriter::new(sites_file), sites) {
		Ok(_) => Ok(()),
		Err(e) => Err(io::Error::new(ErrorKind::Other, e)),
	}
}
