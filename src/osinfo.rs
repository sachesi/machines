//! Which system an installation ISO holds, and what it needs, from the osinfo database that
//! virt-manager and GNOME Boxes use. Its XML is read as it is, without libosinfo.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use crate::glib;

/// Where ISO 9660 keeps its primary volume descriptor.
const PVD_OFFSET: u64 = 16 * 2048;
/// How far `derives-from` is followed for what an entry does not say itself.
const MAX_ANCESTORS: usize = 8;

/// A system the database knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Os {
    /// Its URI, such as `http://fedoraproject.org/fedora/42`.
    pub id: String,
    pub name: String,
    /// `linux`, `winnt`, `freebsd`…
    pub family: String,
    pub resources: Resources,
    pub firmware: FirmwareNeed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Resources {
    pub ram: Option<u64>,
    pub storage: Option<u64>,
    pub cpus: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FirmwareNeed {
    #[default]
    Either,
    Uefi,
    Bios,
}

/// What an ISO says of itself in its volume descriptor, which the database's media match.
#[derive(Debug, Default, PartialEq, Eq)]
struct Volume {
    system: String,
    volume: String,
    publisher: String,
    application: String,
}

/// One entry of the database, before `derives-from` fills in what it leaves out.
#[derive(Debug, Default)]
struct Entry {
    id: String,
    name: String,
    family: String,
    derives_from: Option<String>,
    media: Vec<Media>,
    resources: Option<Resources>,
    firmware: Option<FirmwareNeed>,
}

/// Regular expressions an ISO's fields have to match.
#[derive(Debug, Default)]
struct Media {
    system: Option<String>,
    volume: Option<String>,
    publisher: Option<String>,
    application: Option<String>,
}

/// The system on the ISO at `iso`, if it is one the database knows.
pub fn identify(iso: &Path) -> Option<Os> {
    let volume = read_volume(iso)?;
    let entries = load(None);
    // Where several entries match, the one whose media name the most fields is the surest.
    let entry = entries
        .values()
        .filter_map(|e| {
            let best = e
                .media
                .iter()
                .filter(|m| m.matches(&volume))
                .map(Media::fields)
                .max()?;
            Some((best, e))
        })
        .max_by(|(a, x), (b, y)| a.cmp(b).then_with(|| x.id.cmp(&y.id)))?
        .1;
    Some(resolve(entry, &entries))
}

/// The name of the system the osinfo id `id` names, such as "Fedora Linux 41", where the
/// database has it.
pub fn name(id: &str) -> Option<String> {
    static NAMES: LazyLock<Mutex<HashMap<String, Option<String>>>> = LazyLock::new(Mutex::default);
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    names
        .entry(id.to_owned())
        .or_insert_with(|| {
            // Each vendor's systems are in a directory named for the host part of their ids.
            let vendor = id.split_once("://")?.1.split('/').next()?;
            if vendor.is_empty() || vendor.starts_with('.') {
                return None;
            }
            load(Some(vendor))
                .remove(id)
                .map(|e| e.name)
                .filter(|n| !n.is_empty())
        })
        .clone()
}

fn read_volume(iso: &Path) -> Option<Volume> {
    let mut file = File::open(iso).ok()?;
    file.seek(SeekFrom::Start(PVD_OFFSET)).ok()?;
    let mut pvd = [0u8; 2048];
    file.read_exact(&mut pvd).ok()?;
    if pvd[0] != 1 || &pvd[1..6] != b"CD001" {
        return None;
    }
    let field = |range: std::ops::Range<usize>| {
        String::from_utf8_lossy(&pvd[range])
            .trim_end_matches([' ', '\0'])
            .to_owned()
    };
    Some(Volume {
        system: field(8..40),
        volume: field(40..72),
        publisher: field(318..446),
        application: field(574..702),
    })
}

impl Media {
    fn fields(&self) -> usize {
        [
            &self.system,
            &self.volume,
            &self.publisher,
            &self.application,
        ]
        .into_iter()
        .filter(|f| f.is_some())
        .count()
    }

    fn matches(&self, volume: &Volume) -> bool {
        if self.volume.is_none() && self.system.is_none() && self.publisher.is_none() {
            return false;
        }
        [
            (&self.system, &volume.system),
            (&self.volume, &volume.volume),
            (&self.publisher, &volume.publisher),
            (&self.application, &volume.application),
        ]
        .into_iter()
        .all(|(pattern, value)| {
            pattern.as_deref().is_none_or(|p| {
                glib::Regex::match_simple(
                    p,
                    value,
                    glib::RegexCompileFlags::empty(),
                    glib::RegexMatchFlags::empty(),
                )
            })
        })
    }
}

/// The database's directories, in the order libosinfo reads them: later ones override
/// entries of earlier ones.
fn directories() -> Vec<PathBuf> {
    let system = std::env::var_os("OSINFO_SYSTEM_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/share/osinfo"));
    vec![
        system,
        PathBuf::from("/etc/osinfo"),
        glib::user_config_dir().join("osinfo"),
    ]
}

/// The entries of the database, by id: `vendor`'s, or every vendor's.
fn load(vendor: Option<&str>) -> HashMap<String, Entry> {
    let mut entries = HashMap::new();
    for dir in directories() {
        let os = dir.join("os");
        let vendors: Vec<PathBuf> = match vendor {
            Some(vendor) => vec![os.join(vendor)],
            None => std::fs::read_dir(&os)
                .into_iter()
                .flatten()
                .flatten()
                .map(|v| v.path())
                .collect(),
        };
        for file in vendors
            .iter()
            .filter_map(|v| std::fs::read_dir(v).ok())
            .flatten()
            .flatten()
        {
            let Ok(xml) = std::fs::read_to_string(file.path()) else {
                continue;
            };
            for entry in parse(&xml) {
                entries.insert(entry.id.clone(), entry);
            }
        }
    }
    entries
}

fn parse(xml: &str) -> Vec<Entry> {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return Vec::new();
    };
    doc.root_element()
        .children()
        .filter(|n| n.has_tag_name("os"))
        .map(|os| {
            let text = |tag: &str| {
                os.children()
                    .find(|c| {
                        c.has_tag_name(tag)
                            && c.attribute(("http://www.w3.org/XML/1998/namespace", "lang"))
                                .is_none()
                    })
                    .and_then(|c| c.text())
                    .unwrap_or_default()
                    .trim()
                    .to_owned()
            };
            let ours = |n: &roxmltree::Node| {
                matches!(
                    n.attribute("arch"),
                    None | Some("x86_64" | "i686" | "i386" | "all")
                )
            };
            Entry {
                id: os.attribute("id").unwrap_or_default().to_owned(),
                name: text("name"),
                family: text("family"),
                derives_from: os
                    .children()
                    .find(|c| c.has_tag_name("derives-from"))
                    .and_then(|c| c.attribute("id"))
                    .map(str::to_owned),
                media: os
                    .children()
                    .filter(|c| c.has_tag_name("media") && ours(c))
                    .filter_map(|m| m.children().find(|c| c.has_tag_name("iso")))
                    .map(|iso| {
                        let pattern = |tag: &str| {
                            iso.children()
                                .find(|c| c.has_tag_name(tag))
                                .and_then(|c| c.text())
                                .map(str::to_owned)
                        };
                        Media {
                            system: pattern("system-id"),
                            volume: pattern("volume-id"),
                            publisher: pattern("publisher-id"),
                            application: pattern("application-id"),
                        }
                    })
                    .collect(),
                resources: resources(&os),
                firmware: firmware(&os),
            }
        })
        .collect()
}

/// What the entry recommends of the host for x86-64, else the least it needs.
fn resources(os: &roxmltree::Node) -> Option<Resources> {
    let mut all: Vec<_> = os
        .children()
        .filter(|c| {
            c.has_tag_name("resources") && matches!(c.attribute("arch"), Some("x86_64" | "all"))
        })
        .collect();
    // An entry for x86-64 itself wins over one for all architectures.
    all.sort_by_key(|r| r.attribute("arch") != Some("x86_64"));
    let value = |tag: &str| {
        ["recommended", "minimum"].into_iter().find_map(|level| {
            all.iter().find_map(|r| {
                r.children()
                    .find(|c| c.has_tag_name(level))?
                    .children()
                    .find(|c| c.has_tag_name(tag))?
                    .text()?
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
        })
    };
    let found = Resources {
        ram: value("ram"),
        storage: value("storage"),
        cpus: value("n-cpus").and_then(|n| u32::try_from(n).ok()),
    };
    (found != Resources::default()).then_some(found)
}

fn firmware(os: &roxmltree::Node) -> Option<FirmwareNeed> {
    let unsupported = |kind: &str| {
        os.children().any(|c| {
            c.has_tag_name("firmware")
                && c.attribute("arch") == Some("x86_64")
                && c.attribute("type") == Some(kind)
                && c.attribute("supported") == Some("false")
        })
    };
    let listed = os
        .children()
        .any(|c| c.has_tag_name("firmware") && c.attribute("arch") == Some("x86_64"));
    if unsupported("bios") {
        Some(FirmwareNeed::Uefi)
    } else if unsupported("efi") {
        Some(FirmwareNeed::Bios)
    } else {
        listed.then_some(FirmwareNeed::Either)
    }
}

/// `entry` with what it leaves out taken from the entries it derives from.
fn resolve(entry: &Entry, entries: &HashMap<String, Entry>) -> Os {
    let ancestry = std::iter::successors(Some(entry), |e| {
        e.derives_from.as_ref().and_then(|id| entries.get(id))
    })
    .take(MAX_ANCESTORS);
    let mut resources = None;
    let mut firmware = None;
    for e in ancestry {
        resources = resources.or(e.resources);
        firmware = firmware.or(e.firmware);
    }
    Os {
        id: entry.id.clone(),
        name: entry.name.clone(),
        family: entry.family.clone(),
        resources: resources.unwrap_or_default(),
        firmware: firmware.unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DB: &str = r#"<?xml version="1.0"?>
<libosinfo version="0.0.1">
  <os id="http://microsoft.com/win/10">
    <name>Microsoft Windows 10</name>
    <family>winnt</family>
    <resources arch="all">
      <minimum><n-cpus>1</n-cpus><ram>1073741824</ram></minimum>
      <recommended><ram>2147483648</ram><storage>21474836480</storage></recommended>
    </resources>
  </os>
  <os id="http://microsoft.com/win/11">
    <name>Microsoft Windows 11</name>
    <name xml:lang="de">Microsoft Windows 11 (de)</name>
    <family>winnt</family>
    <derives-from id="http://microsoft.com/win/10"/>
    <firmware arch="x86_64" type="efi"/>
    <firmware arch="x86_64" type="bios" supported="false"/>
    <media arch="aarch64">
      <iso><volume-id>^(J_)?(CCSN?A|C?CCOMA)_A64FRE?_</volume-id></iso>
    </media>
    <media arch="x86_64">
      <iso>
        <volume-id>^(J_)?(CCSN?A|C?CCOMA)_X64FREE?_</volume-id>
        <publisher-id>MICROSOFT CORPORATION</publisher-id>
      </iso>
    </media>
  </os>
</libosinfo>"#;

    fn db() -> HashMap<String, Entry> {
        parse(DB).into_iter().map(|e| (e.id.clone(), e)).collect()
    }

    #[test]
    fn media_match_all_the_fields_they_name() {
        let db = db();
        let win11 = &db["http://microsoft.com/win/11"];
        assert_eq!(win11.name, "Microsoft Windows 11");
        assert_eq!(win11.media.len(), 1);
        let mut volume = Volume {
            volume: "CCCOMA_X64FRE_EN-US_DV9".to_owned(),
            publisher: "MICROSOFT CORPORATION".to_owned(),
            ..Volume::default()
        };
        assert!(win11.media[0].matches(&volume));
        volume.publisher = "SOMEONE ELSE".to_owned();
        assert!(!win11.media[0].matches(&volume));
    }

    #[test]
    fn what_an_entry_leaves_out_comes_from_the_one_it_derives_from() {
        let db = db();
        let os = resolve(&db["http://microsoft.com/win/11"], &db);
        assert_eq!(os.firmware, FirmwareNeed::Uefi);
        assert_eq!(
            os.resources,
            Resources {
                ram: Some(2147483648),
                storage: Some(21474836480),
                cpus: Some(1),
            }
        );
        let win10 = resolve(&db["http://microsoft.com/win/10"], &db);
        assert_eq!(win10.firmware, FirmwareNeed::Either);
    }
}
