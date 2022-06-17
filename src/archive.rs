use serde::Deserialize;

pub fn parse<R, T, F, FT, C>(
    archive: &mut zip::ZipArchive<R>,
    datafile: &str,
    mut map: F,
) -> anyhow::Result<C>
where
    R: std::io::Read + std::io::Seek,
    T: serde::de::DeserializeOwned,
    F: FnMut(T) -> Option<FT>,
    C: FromIterator<FT>,
{
    use anyhow::Context;
    let mut file = archive
        .by_name(datafile)
        .with_context(|| format!("pick {} from archive", datafile))?;
    let mut contents = String::new();
    std::io::Read::read_to_string(&mut file, &mut contents)
        .with_context(|| format!("read {}", datafile))?;

    let data_start = contents
        .find('[')
        .with_context(|| format!("find [ indicating start of data in {}", datafile))?;
    let data = &contents[data_start..];
    let mut data = data.as_bytes();
    let deser = serde_json_array_iter::iter_json_array(&mut data);
    Ok(deser
        .filter_map(|v| v.map(&mut map).transpose())
        .collect::<Result<C, std::io::Error>>()
        .with_context(|| format!("parse {}", datafile))?)
}

#[derive(Debug, Deserialize)]
pub enum Follower {
    #[serde(rename = "follower")]
    One {
        #[serde(rename = "accountId", deserialize_with = "u64_from_str")]
        id: u64,
    },
}

#[derive(Debug, Deserialize)]
pub enum Tweet {
    #[serde(rename = "tweet")]
    One {
        #[serde(rename = "id", deserialize_with = "u64_from_str")]
        id: u64,
        #[serde(rename = "full_text")]
        text: String,
    },
}

fn u64_from_str<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    s.parse().map_err(serde::de::Error::custom)
}

// https://github.com/serde-rs/json/issues/404
mod serde_json_array_iter {
    use serde::de::DeserializeOwned;
    use serde_json::{self, Deserializer};
    use std::io::{self, Read};

    fn read_skipping_ws(mut reader: impl Read) -> io::Result<u8> {
        loop {
            let mut byte = 0u8;
            reader.read_exact(std::slice::from_mut(&mut byte))?;
            if !byte.is_ascii_whitespace() {
                return Ok(byte);
            }
        }
    }

    fn invalid_data(msg: &str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, msg)
    }

    fn deserialize_single<T: DeserializeOwned, R: Read>(reader: R) -> io::Result<T> {
        let next_obj = Deserializer::from_reader(reader).into_iter::<T>().next();
        match next_obj {
            Some(result) => result.map_err(Into::into),
            None => Err(invalid_data("premature EOF")),
        }
    }

    fn yield_next_obj<T: DeserializeOwned, R: Read>(
        mut reader: R,
        at_start: &mut bool,
    ) -> io::Result<Option<T>> {
        if !*at_start {
            *at_start = true;
            if read_skipping_ws(&mut reader)? == b'[' {
                // read the next char to see if the array is empty
                let peek = read_skipping_ws(&mut reader)?;
                if peek == b']' {
                    Ok(None)
                } else {
                    deserialize_single(io::Cursor::new([peek]).chain(reader)).map(Some)
                }
            } else {
                Err(invalid_data("`[` not found"))
            }
        } else {
            match read_skipping_ws(&mut reader)? {
                b',' => deserialize_single(reader).map(Some),
                b']' => Ok(None),
                _ => Err(invalid_data("`,` or `]` not found")),
            }
        }
    }

    pub fn iter_json_array<T: DeserializeOwned, R: Read>(
        mut reader: R,
    ) -> impl Iterator<Item = Result<T, io::Error>> {
        let mut at_start = false;
        std::iter::from_fn(move || yield_next_obj(&mut reader, &mut at_start).transpose())
    }
}
