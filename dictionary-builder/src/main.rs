//! A simple application that downloads the data dictionary
//! from the latest DICOM standard found online, then creates
//! code or data to reproduce it in the core library.
//!
//! ### How to use
//!
//! Simply run the application. It will automatically retrieve the dictionary
//! from the official DICOM website and store the result in "entries.rs".
//! Future versions will enable different kinds of outputs.
//!
//! Please use the `--help` flag for the full usage information.

use clap::{App, Arg};
use futures::{Future, Stream};
use hyper::client::Client;
use hyper::client::ResponseFuture;
use hyper::{Chunk, Uri};
use serde::Serialize;
use serde_json::to_writer;
use tokio_core::reactor::Core;

use quick_xml::events::attributes::Attribute;
use quick_xml::events::Event;
use quick_xml::Error as XmlError;
use quick_xml::Reader;
use regex::Regex;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::{create_dir_all, File};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::str::FromStr;

/// url to PS3.6 XML file
const DEFAULT_LOCATION: &str =
    "http://dicom.nema.org/medical/dicom/current/source/docbook/part06/part06.xml";

fn main() {
    let matches = App::new("DICOM Dictionary Builder")
        .version("0.1.0")
        .arg(
            Arg::with_name("FROM")
                .default_value(DEFAULT_LOCATION)
                .help("Where to fetch the dictionary from"),
        )
        .arg(
            Arg::with_name("OUTPUT")
                .short("o")
                .help("The path to the output file")
                .required(false)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("FORMAT")
                .short("f")
                .help("The output format")
                .required(true)
                .default_value("rs")
                .takes_value(true)
                .possible_value("rs")
                .possible_value("json"),
        )
        .arg(
            Arg::with_name("no-retired")
                .help("Whether to ignore retired tags")
                .takes_value(false),
        )
        .get_matches();

    let format = matches.value_of("FORMAT").unwrap();
    let ignore_retired = matches.is_present("no-retired");

    let out_file = matches.value_of("OUTPUT").unwrap_or_else(|| match format {
        "rs" => "entries.rs",
        "json" => "entries.json",
        _ => "entries",
    });
    let dst = Path::new(out_file);

    let mut core = Core::new().unwrap();

    let src = matches.value_of("FROM").unwrap();
    if src.starts_with("http:") || src.starts_with("https:") {
        let src = Uri::from_str(src).unwrap();
        println!("Downloading DICOM dictionary ...");
        let req = xml_from_site(src).and_then(|resp| {
            resp.into_body().concat2().and_then(|body: Chunk| {
                let xml_entries = XmlEntryIterator::new(&*body).map(|item| item.unwrap());
                println!("Writing to file ...");
                match format {
                    "rs" => to_code_file(dst, xml_entries, !ignore_retired),
                    "json" => to_json_file(dst, xml_entries),
                    _ => unreachable!(),
                }
                .expect("Failed to write file");
                Ok(())
            })
        });
        core.run(req).unwrap();
    } else {
        // read from File
        let file = File::open(src).unwrap();
        let file = BufReader::new(file);
        let xml_entries = XmlEntryIterator::new(file).map(|item| item.unwrap());

        match format {
            "rs" => to_code_file(dst, xml_entries, true),
            "json" => to_json_file(dst, xml_entries),
            _ => unreachable!(),
        }
        .expect("Failed to write file");
    }
}

type XmlResult<T> = Result<T, XmlError>;
type DynResult<T> = Result<T, Box<dyn std::error::Error>>;

fn xml_from_site(url: Uri) -> ResponseFuture {
    let client = Client::new();
    client.get(url)
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Clone, Serialize)]
struct Entry {
    tag: String,
    name: Option<String>,
    alias: Option<String>,
    vr: Option<String>,
    vm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    obs: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum XmlReadingState {
    Off,
    InTableHead,
    InTable,
    InCellTag,
    InCellName,
    InCellKeyword,
    InCellVR,
    InCellVM,
    InCellObs,
    InCellUnknown,
}

struct XmlEntryIterator<R: BufRead> {
    parser: Reader<R>,
    buf: Vec<u8>,
    depth: u32,
    tag: Option<String>,
    name: Option<String>,
    keyword: Option<String>,
    vr: Option<String>,
    vm: Option<String>,
    obs: Option<String>,
    state: XmlReadingState,
}

impl<R: BufRead> XmlEntryIterator<R> {
    pub fn new(xml: R) -> XmlEntryIterator<R> {
        let mut reader = Reader::from_reader(xml);
        reader.expand_empty_elements(true).trim_text(true);
        XmlEntryIterator {
            parser: reader,
            buf: Vec::new(),
            depth: 0,
            tag: None,
            name: None,
            keyword: None,
            vr: None,
            vm: None,
            obs: None,
            state: XmlReadingState::Off,
        }
    }
}

impl<R: BufRead> Iterator for XmlEntryIterator<R> {
    type Item = XmlResult<Entry>;
    fn next(&mut self) -> Option<XmlResult<Entry>> {
        loop {
            self.buf.clear();
            let res = self.parser.read_event(&mut self.buf);
            match res {
                Ok(Event::Start(ref e)) => {
                    self.depth += 1;
                    let local_name = e.local_name();
                    match self.state {
                        XmlReadingState::Off => {
                            if local_name == b"table" {
                                // check for attribute xml:id="table_6-1"
                                match e.attributes().find(|attr| {
                                    attr.is_err()
                                        || attr.as_ref().unwrap()
                                            == &Attribute {
                                                key: b"xml:id",
                                                value: Cow::Borrowed(b"table_6-1"),
                                            }
                                }) {
                                    Some(Ok(_)) => {
                                        // entered the table!
                                        self.state = XmlReadingState::InTableHead;
                                    }
                                    Some(Err(err)) => return Some(Err(err)),
                                    None => {}
                                }
                            }
                        }
                        XmlReadingState::InTableHead => {
                            if local_name == b"tbody" {
                                self.state = XmlReadingState::InTable;
                            }
                        }
                        XmlReadingState::InTable => {
                            if local_name == b"para" {
                                self.state = XmlReadingState::InCellTag;
                            }
                        }
                        XmlReadingState::InCellTag => {
                            if local_name == b"para" {
                                self.state = XmlReadingState::InCellName;
                            }
                        }
                        XmlReadingState::InCellName => {
                            if local_name == b"para" {
                                self.state = XmlReadingState::InCellKeyword;
                            }
                        }
                        XmlReadingState::InCellKeyword => {
                            if local_name == b"para" {
                                self.state = XmlReadingState::InCellVR;
                            }
                        }
                        XmlReadingState::InCellVR => {
                            if local_name == b"para" {
                                self.state = XmlReadingState::InCellVM;
                            }
                        }
                        XmlReadingState::InCellVM => {
                            if local_name == b"para" {
                                self.state = XmlReadingState::InCellObs;
                            }
                        }
                        XmlReadingState::InCellObs => {
                            if local_name == b"para" {
                                self.state = XmlReadingState::InCellUnknown;
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::End(ref e)) => {
                    self.depth -= 1;
                    let local_name = e.local_name();
                    match self.state {
                        XmlReadingState::Off => {
                            // do nothing
                        }
                        _e => {
                            if local_name == b"tr" && self.tag.is_some() {
                                let tag = self.tag.take().unwrap();
                                let out = Entry {
                                    tag,
                                    name: self.name.take(),
                                    alias: self.keyword.take(),
                                    vr: self.vr.take(),
                                    vm: self.vm.take(),
                                    obs: self.obs.take(),
                                };
                                self.state = XmlReadingState::InTable;
                                return Some(Ok(out));
                            } else if local_name == b"tbody" {
                                // the table ended!
                                break;
                            }
                        }
                    }
                }
                Ok(Event::Text(data)) => match self.state {
                    XmlReadingState::InCellTag => {
                        let data = data
                            .unescape_and_decode(&self.parser)
                            .unwrap()
                            .replace("\u{200b}", "");
                        self.tag = Some(data);
                    }
                    XmlReadingState::InCellName => {
                        let data = data
                            .unescape_and_decode(&self.parser)
                            .unwrap()
                            .replace("\u{200b}", "");
                        self.name = Some(data);
                    }
                    XmlReadingState::InCellKeyword => {
                        let data = data
                            .unescape_and_decode(&self.parser)
                            .unwrap()
                            .replace("\u{200b}", "");
                        self.keyword = Some(data);
                    }
                    XmlReadingState::InCellVR => {
                        let data = data
                            .unescape_and_decode(&self.parser)
                            .unwrap()
                            .replace("\u{200b}", "");
                        self.vr = Some(data);
                    }
                    XmlReadingState::InCellVM => {
                        let data = data
                            .unescape_and_decode(&self.parser)
                            .unwrap()
                            .replace("\u{200b}", "");
                        self.vm = Some(data);
                    }
                    XmlReadingState::InCellObs => {
                        let data = data
                            .unescape_and_decode(&self.parser)
                            .unwrap()
                            .replace("\u{200b}", "");
                        self.obs = Some(data);
                    }
                    _ => {}
                },
                Ok(Event::Eof { .. }) => {
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    return Some(Err(e));
                }
            }
        }

        None
    }
}

fn to_code_file<P: AsRef<Path>, I>(dest_path: P, entries: I, include_retired: bool) -> DynResult<()>
where
    I: IntoIterator<Item = Entry>,
{
    if let Some(p_dir) = dest_path.as_ref().parent() {
        create_dir_all(&p_dir)?;
    }
    let mut f = File::create(&dest_path)?;

    f.write_all(
        b"//! Automatically generated. Edit at your own risk.\n\n\
    use dicom_core::dictionary::{DictionaryEntryRef, TagRange::*};\n\
    use dicom_core::Tag;\n\
    use dicom_core::VR::*;\n\n\
    type E = DictionaryEntryRef<'static>;\n\n\
    #[rustfmt::skip]\n\
    pub const ENTRIES: &[E] = &[\n",
    )?;

    let regex_tag = Regex::new(r"^\(([0-9A-F]{4}),([0-9A-F]{4})\)$")?;
    let regex_tag_group100 = Regex::new(r"^\(([0-9A-F]{2})xx,([0-9A-F]{4})\)$")?;
    let regex_tag_element100 = Regex::new(r"^\(([0-9A-F]{4}),([0-9A-F]{2})xx\)$")?;

    for e in entries {
        let Entry {
            tag,
            alias,
            vr,
            obs,
            ..
        } = e;

        // sanitize components

        let alias = if let Some(v) = alias {
            v
        } else {
            continue;
        };

        if let Some(ref s) = obs {
            if s == "RET" && !include_retired {
                // don't include retired attributes
                continue;
            }
        }
        let cap = regex_tag.captures(tag.as_str());
        let tag_txt = if let Some(cap) = cap {
            // single tag
            let group = cap.get(1).expect("capture group 1: group").as_str();
            let elem = cap.get(2).expect("capture group 2: element").as_str();
            format!("Single(Tag(0x{}, 0x{}))", group, elem)
        } else if let Some(cap) = regex_tag_group100.captures(tag.as_str()) {
            // tag range over groups: (ggxx, eeee)
            let group = cap.get(1).expect("capture group 1: group portion").as_str();
            let elem = cap.get(2).expect("capture group 2: element").as_str();
            format!("Group100(Tag(0x{}00, 0x{}))", group, elem)
        } else if let Some(cap) = regex_tag_element100.captures(tag.as_str()) {
            // tag range over elements: (gggg, eexx)
            let group = cap.get(1).expect("capture group 1: group").as_str();
            let elem = cap
                .get(2)
                .expect("capture group 2: element portion")
                .as_str();
            format!("Element100(Tag(0x{}, 0x{}00))", group, elem)
        } else {
            continue;
        };

        let mut vr = vr.unwrap_or_else(|| "".into());
        if vr == "See Note" {
            vr = "UN See Note".to_string();
        }

        let (vr1, vr2) = vr.split_at(2);

        let second_vr = if vr2 != "" {
            format!(" /*{} */", vr2)
        } else {
            vr2.to_string()
        };

        let mut obs = obs.unwrap_or_else(String::new);
        if obs != "" {
            obs = format!(" // {}", obs.as_str());
        }

        writeln!(
            f,
            "    E {{ tag: {}, alias: \"{}\", vr: {}{} }},{}",
            tag_txt, alias, vr1, second_vr, obs
        )?;
    }
    f.write_all(b"];\n")?;
    Ok(())
}

fn to_json_file<P: AsRef<Path>, I>(dest_path: P, entries: I) -> DynResult<()>
where
    I: IntoIterator<Item = Entry>,
{
    if let Some(p_dir) = dest_path.as_ref().parent() {
        create_dir_all(&p_dir)?;
    }
    let f = File::create(&dest_path)?;

    let entries: BTreeMap<String, Entry> =
        entries.into_iter().map(|v| (v.tag.clone(), v)).collect();

    to_writer(f, &entries)?;
    Ok(())
}
