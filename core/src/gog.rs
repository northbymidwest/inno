//! Reassembling the files in a GOG.com installer.
//!
//! GOG builds its installers so that no single entry is larger than a fixed
//! part size. Every payload file is stored in the temporary directory under a
//! content-addressed name, and the name it is meant to end up with appears
//! nowhere in the file entry itself:
//!
//! ```text
//! destination      {tmp}\43\c4\43c42d97cd4395cafc5b251378f5be5a
//! before_install   before_install('5538b1...', 'LOCO.EXE', 1)
//! after_install    after_install('5538b1...', 1533091, 3117056)
//! ```
//!
//! The script called before installing a file names it and says how many
//! parts it was split into. That entry is the first part; the parts after it
//! are the entries that follow, which carry no `before_install` of their own.
//! Concatenating them in order reproduces the file.
//!
//! Without this an installer reads as a few thousand files with hexadecimal
//! names, none of them usable, which is what every entry in one of these
//! installers looks like on its own.

use std::io::{self, Read};

use flate2::read::ZlibDecoder;

use crate::entry::File;

/// A file the installer produces, and the entries it is made from.
///
/// Most files in a GOG.com installer are split into parts stored under
/// content-addressed names, but not all: an installer mixes those with
/// ordinary entries that carry their own destination. Both appear here, and
/// [`assemble`] handles the difference, because treating the second kind as
/// though it did not exist quietly loses real files. In Chris Sawyer's
/// Locomotion those are `Data/plugin.dat` and the saved games.
///
/// [`assemble`]: Self::assemble
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallerFile {
    root: Option<String>,
    path: String,
    checksum: Option<String>,
    parts: Vec<usize>,
    compressed: bool,
}

impl InstallerFile {
    /// Returns the path the file is meant to be written to, relative to
    /// [`root`], with `/` separators.
    ///
    /// [`root`]: Self::root
    #[must_use]
    #[inline]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the directory constant the file is written under, without its
    /// braces: `app` for the installed program, `tmp` for the files the
    /// installer only needs while it runs, and so on.
    ///
    /// The path alone is not unique. An installer that writes the same
    /// filename under two constants, which GOG's do for their icon, gives two
    /// files with identical paths and different contents; keeping only the
    /// path silently turns those into one file.
    ///
    /// Files split into parts report `app`, which is where the script that
    /// names them puts them.
    #[must_use]
    #[inline]
    pub fn root(&self) -> Option<&str> {
        self.root.as_deref()
    }

    /// Returns the MD5 of the finished file as the installer records it, in
    /// lower-case hexadecimal.
    ///
    /// This is what the installer names the file by internally, and it is
    /// over the reassembled and decompressed contents, so it checks the work
    /// of every step here at once.
    #[must_use]
    #[inline]
    pub fn checksum(&self) -> Option<&str> {
        self.checksum.as_deref()
    }

    /// Returns the indices, into the installer's file entries, of the parts
    /// this file is split across, in the order they must be concatenated.
    #[must_use]
    #[inline]
    pub fn parts(&self) -> &[usize] {
        &self.parts
    }

    /// Builds the finished file from the bytes of its parts, which must be
    /// given in the order [`parts`] lists them.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] if the number of parts does
    /// not match, and the decoder's error if a part that should be a zlib
    /// stream is not one.
    ///
    /// [`parts`]: Self::parts
    pub fn assemble(&self, parts: &[Vec<u8>]) -> io::Result<Vec<u8>> {
        if parts.len() != self.parts.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is {} parts, given {}",
                    self.path,
                    self.parts.len(),
                    parts.len()
                ),
            ));
        }

        let mut out = Vec::new();
        for part in parts {
            if self.compressed {
                out.extend_from_slice(&decompress_part(part)?);
            } else {
                out.extend_from_slice(part);
            }
        }
        Ok(out)
    }
}

/// Decompresses one part of a GOG.com file.
///
/// Each part is a zlib stream of its own, which is not something the Inno
/// Setup file entry says: the entry's own compression filter reads as none,
/// because this compression is GOG's and is applied before the installer ever
/// sees the data. A part therefore comes out of the installer still
/// compressed, and concatenating parts without this produces a file of the
/// right shape and the wrong contents.
///
/// # Errors
///
/// Returns the decoder's error if `part` is not a valid zlib stream.
fn decompress_part(part: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    ZlibDecoder::new(part).read_to_end(&mut out)?;
    Ok(out)
}

/// Returns the files a GOG.com installer is meant to produce.
///
/// An installer that does not use GOG's scheme has no `before_install`
/// markers and produces an empty result, which is how a caller tells the two
/// apart: there is nothing to reassemble and the entries' own destinations
/// are already the answer.
#[must_use]
pub fn files(entries: &[File]) -> Vec<InstallerFile> {
    plan(
        entries
            .iter()
            .map(|entry| (entry.condition().before_install(), entry.destination())),
    )
}

/// The part of [`files`] that does not need a whole installer to test: each
/// entry's `before_install` script and its own destination, in order.
fn plan<'a>(
    entries: impl Iterator<Item = (Option<&'a str>, Option<&'a str>)>,
) -> Vec<InstallerFile> {
    let mut files: Vec<InstallerFile> = Vec::new();
    let mut remaining = 0_usize;

    for (index, (script, destination)) in entries.enumerate() {
        if let Some(start) = script.and_then(start_of_file) {
            // A new file begins here whether or not the one before it got
            // all the parts it asked for. A count that overran would
            // otherwise swallow the next file's first part, turning one
            // wrong file into two.
            files.push(InstallerFile {
                root: Some("app".to_string()),
                path: start.path,
                checksum: start.checksum,
                parts: vec![index],
                compressed: true,
            });
            remaining = start.parts.saturating_sub(1);
            continue;
        }

        if remaining > 0
            && let Some(current) = files.last_mut()
        {
            current.parts.push(index);
            remaining -= 1;
            continue;
        }

        // Not part of a split file, so it is a file in its own right, stored
        // and named the ordinary way. An entry with nowhere to go is not a
        // file at all.
        if let Some((root, path)) = destination.map(split_root)
            && !path.is_empty()
        {
            files.push(InstallerFile {
                root,
                path,
                checksum: None,
                parts: vec![index],
                compressed: false,
            });
        }
    }

    files
}

struct Start {
    path: String,
    checksum: Option<String>,
    parts: usize,
}

/// Reads `before_install('<id>', '<path>', <parts>)`, and the `_dependency`
/// spelling used for the redistributables GOG ships alongside the game.
fn start_of_file(script: &str) -> Option<Start> {
    let arguments = call_arguments(script, "before_install")
        .or_else(|| call_arguments(script, "before_install_dependency"))?;

    let path = arguments.get(1)?;
    if path.is_empty() {
        return None;
    }

    // A part count that is missing or unreadable means one part. Dropping the
    // file instead would lose it entirely over a detail that only matters for
    // files large enough to be split.
    let parts = arguments
        .get(2)
        .and_then(|count| count.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);

    Some(Start {
        path: normalize(path),
        checksum: arguments.first().map(|id| id.to_lowercase()),
        parts,
    })
}

/// Turns a Windows path from the installer script into the form the rest of
/// this crate uses for destinations.
fn normalize(path: &str) -> String {
    path.replace('\\', "/")
}

/// Splits `{app}\\Data\\x.dat` into its directory constant and the path
/// under it. A destination with no constant keeps its whole path and has no
/// root.
fn split_root(destination: &str) -> (Option<String>, String) {
    let Some(rest) = destination.strip_prefix('{') else {
        return (None, normalize(destination));
    };

    let Some(end) = rest.find('}') else {
        return (None, normalize(destination));
    };

    let path = rest[end + 1..]
        .strip_prefix('\\')
        .or_else(|| rest[end + 1..].strip_prefix('/'))
        .unwrap_or(&rest[end + 1..]);

    (Some(rest[..end].to_string()), normalize(path))
}

/// Reads the arguments of `name(...)` out of a fragment of the Pascal that
/// Inno Setup scripts are written in.
///
/// Returns `None` unless the fragment is a call to exactly that function, so
/// that `before_install_dependency` is not mistaken for `before_install`.
/// Arguments are returned with their quotes removed and `''` unescaped;
/// unquoted ones, which is how the numbers appear, are returned as written.
fn call_arguments(code: &str, name: &str) -> Option<Vec<String>> {
    let code = code.trim_start();
    let rest = code.strip_prefix(name)?;
    let rest = rest.trim_start();
    let mut characters = rest.strip_prefix('(')?.chars().peekable();

    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut started = false;

    while let Some(character) = characters.next() {
        match character {
            '\'' if quoted => {
                // Two quotes in a row are one quote in the string, which is
                // how Pascal escapes them.
                if characters.peek() == Some(&'\'') {
                    characters.next();
                    current.push('\'');
                } else {
                    quoted = false;
                }
            }
            '\'' => {
                quoted = true;
                started = true;
            }
            _ if quoted => current.push(character),
            ',' => {
                arguments.push(std::mem::take(&mut current).trim().to_string());
                started = false;
            }
            ')' => {
                if started || !current.trim().is_empty() {
                    arguments.push(current.trim().to_string());
                }
                return Some(arguments);
            }
            _ => {
                current.push(character);
                started = true;
            }
        }
    }

    // Unterminated: the call was cut off, so nothing here can be trusted.
    None
}

#[cfg(test)]
mod tests {
    use super::{call_arguments, plan};

    #[test]
    fn a_call_gives_up_its_arguments() {
        let arguments =
            call_arguments("before_install('5538b1', 'LOCO.EXE', 1)", "before_install").unwrap();
        assert_eq!(arguments, ["5538b1", "LOCO.EXE", "1"]);
    }

    #[test]
    fn a_different_function_with_the_same_prefix_is_not_a_match() {
        // before_install_dependency starts with before_install, and matching
        // on the prefix would file every redistributable under the wrong
        // name.
        assert!(
            call_arguments(
                "before_install_dependency('14afcf', '__redist\\ISI\\x.exe', 1)",
                "before_install"
            )
            .is_none()
        );
    }

    #[test]
    fn a_quote_inside_an_argument_survives() {
        let arguments = call_arguments(
            "before_install('id', 'Sawyer''s Loco.exe', 1)",
            "before_install",
        )
        .unwrap();
        assert_eq!(arguments[1], "Sawyer's Loco.exe");
    }

    #[test]
    fn a_call_that_was_cut_off_is_not_half_read() {
        assert!(call_arguments("before_install('id', 'LOCO.EXE'", "before_install").is_none());
    }

    #[test]
    fn a_single_part_file_is_one_entry() {
        let files = plan([(Some("before_install('id', 'LOCO.EXE', 1)"), None)].into_iter());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path(), "LOCO.EXE");
        assert_eq!(files[0].parts(), [0]);
    }

    #[test]
    fn the_entries_after_a_split_file_are_its_remaining_parts() {
        // Manual.pdf in Chris Sawyer's Locomotion is three parts, and only
        // the first says so.
        let files = plan(
            [
                (Some("before_install('id', 'Manual.pdf', 3)"), None),
                (None, None),
                (None, None),
                (Some("before_install('id2', 'pskill.exe', 1)"), None),
            ]
            .into_iter(),
        );

        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path(), "Manual.pdf");
        assert_eq!(files[0].parts(), [0, 1, 2]);
        assert_eq!(files[1].path(), "pskill.exe");
        assert_eq!(files[1].parts(), [3]);
    }

    #[test]
    fn a_backslash_path_becomes_a_normal_one() {
        let files = plan([(Some(r"before_install('id', 'Data\20s1.dat', 1)"), None)].into_iter());
        assert_eq!(files[0].path(), "Data/20s1.dat");
    }

    #[test]
    fn entries_before_the_first_named_file_belong_to_nothing() {
        // The first entry of a real installer carries no script at all.
        let files = plan(
            [
                (None, None),
                (None, None),
                (Some("before_install('id', 'a.txt', 1)"), None),
            ]
            .into_iter(),
        );
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].parts(), [2]);
    }

    #[test]
    fn a_part_count_that_overruns_does_not_eat_the_next_file() {
        // A count larger than the parts actually present would otherwise
        // take the next file's first entry, losing that file and corrupting
        // this one. Both are wrong; only one of them is quiet.
        let files = plan(
            [
                (Some("before_install('id', 'greedy.bin', 9)"), None),
                (None, None),
                (Some("before_install('id2', 'next.bin', 1)"), None),
            ]
            .into_iter(),
        );

        assert_eq!(files.len(), 2);
        assert_eq!(files[0].parts(), [0, 1]);
        assert_eq!(files[1].path(), "next.bin");
        assert_eq!(files[1].parts(), [2]);
    }

    #[test]
    fn a_part_count_of_zero_still_leaves_the_entry_it_names() {
        // Nothing in the format promises a sensible count, and a file with
        // no parts at all is not a file.
        let files = plan(
            [
                (Some("before_install('id', 'odd.bin', 0)"), None),
                (None, None),
            ]
            .into_iter(),
        );
        assert_eq!(files[0].parts(), [0]);
    }

    #[test]
    fn the_checksum_of_the_finished_file_is_kept() {
        let files = plan(
            [(
                Some("before_install('5538B198F731ABA14ABAF401DDDDF13F', 'LOCO.EXE', 1)"),
                None,
            )]
            .into_iter(),
        );
        assert_eq!(
            files[0].checksum(),
            Some("5538b198f731aba14abaf401ddddf13f"),
            "written upper case by some installers and compared against a \
             lower-case digest"
        );
    }

    #[test]
    fn a_part_round_trips_through_the_zlib_layer() {
        use std::io::Write;

        let original = b"LOCO.EXE would be here";
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(original).unwrap();
        let compressed = encoder.finish().unwrap();

        let file = &plan([(Some("before_install('id', 'x.bin', 1)"), None)].into_iter())[0];
        assert_eq!(file.assemble(&[compressed]).unwrap(), original);
    }

    #[test]
    fn a_part_that_is_not_a_zlib_stream_is_an_error_not_a_panic() {
        let file = &plan([(Some("before_install('id', 'x.bin', 1)"), None)].into_iter())[0];
        assert!(file.assemble(&[b"not compressed at all".to_vec()]).is_err());
    }

    #[test]
    fn an_entry_that_is_not_part_of_a_split_file_is_a_file_of_its_own() {
        // These sit among the split ones in a real installer, and are real
        // game data: Data/plugin.dat and the saved games in Locomotion.
        // Reading only the split files loses them without saying so.
        let files = plan(
            [
                (Some("before_install('id', 'LOCO.EXE', 1)"), None),
                (None, Some(r"{app}\Data\plugin.dat")),
            ]
            .into_iter(),
        );

        assert_eq!(files.len(), 2);
        assert_eq!(files[1].path(), "Data/plugin.dat");
        assert_eq!(files[1].parts(), [1]);
        assert_eq!(files[1].checksum(), None);
    }

    #[test]
    fn a_plain_entry_is_not_decompressed_on_the_way_out() {
        // Only GOG's own parts carry the extra zlib layer. Inflating an
        // ordinary entry would fail on every one of them.
        let files = plan([(None, Some("{app}/readme.txt"))].into_iter());
        assert_eq!(
            files[0].assemble(&[b"plain bytes".to_vec()]).unwrap(),
            b"plain bytes"
        );
    }

    #[test]
    fn an_entry_with_nowhere_to_go_is_not_a_file() {
        let files = plan([(None, None), (None, Some("{app}"))].into_iter());
        assert!(files.is_empty());
    }

    #[test]
    fn assembling_the_wrong_number_of_parts_is_refused() {
        let files = plan([(Some("before_install('id', 'split.bin', 3)"), None)].into_iter());
        assert!(files[0].assemble(&[b"only one".to_vec()]).is_err());
    }

    #[test]
    fn two_files_with_one_name_under_different_roots_stay_two_files() {
        // GOG installers ship their icon under both {app} and {tmp}. Keeping
        // only the path makes them one file, and whichever is written second
        // wins.
        let files = plan(
            [
                (None, Some(r"{app}\goggame.ico")),
                (None, Some(r"{tmp}\goggame.ico")),
            ]
            .into_iter(),
        );

        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path(), files[1].path());
        assert_ne!(files[0].root(), files[1].root());
    }

    #[test]
    fn a_split_file_is_written_under_app() {
        let files = plan([(Some("before_install('id', 'LOCO.EXE', 1)"), None)].into_iter());
        assert_eq!(files[0].root(), Some("app"));
    }

    #[test]
    fn a_destination_with_no_constant_keeps_its_whole_path() {
        let files = plan([(None, Some(r"plain\path.txt"))].into_iter());
        assert_eq!(files[0].root(), None);
        assert_eq!(files[0].path(), "plain/path.txt");
    }

    #[test]
    fn an_installer_that_is_not_gogs_is_read_as_plain_entries() {
        // Every entry carries its own destination and none is split, which
        // is what a plain Inno Setup installer looks like.
        let files = plan(
            [
                (None, Some(r"{app}\a.txt")),
                (None, Some(r"{tmp}\dir\b.txt")),
            ]
            .into_iter(),
        );
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.parts().len() == 1));
        assert_eq!(files[1].path(), "dir/b.txt");
        assert_eq!(files[1].root(), Some("tmp"));
    }
}
