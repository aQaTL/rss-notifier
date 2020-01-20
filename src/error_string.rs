// It's ridiculous I have to do this to return a string as a Error trait object

use std::error::Error;
use std::fmt::{self, Display, Formatter};

#[derive(Debug)]
pub struct ErrorString(pub String);

impl Error for ErrorString {}

impl Display for ErrorString {
	fn fmt(&self, f: &mut Formatter) -> fmt::Result {
		String::fmt(&self.0, f)
	}
}
