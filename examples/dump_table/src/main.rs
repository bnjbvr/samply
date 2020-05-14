use anyhow;
use futures;
use memmap::MmapOptions;
use profiler_get_symbols::{
    self, CompactSymbolTable, FileAndPathHelper, FileAndPathHelperResult, GetSymbolsError,
    OwnedFileData,
};
use std::fs::File;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(
    name = "dump-table",
    about = "Get the symbol table for a debugName + breakpadId identifier."
)]
struct Opt {
    /// debugName identifier
    #[structopt()]
    debug_name: String,

    /// Path to a directory that contains binaries and debug archives
    #[structopt()]
    symbol_directory: PathBuf,

    /// Breakpad ID of the binary
    #[structopt()]
    breakpad_id: Option<String>,

    /// When specified, print the entire symbol table.
    #[structopt(short, long)]
    full: bool,
}

fn main() -> anyhow::Result<()> {
    let opt = Opt::from_args();
    futures::executor::block_on(dump_table(
        &opt.debug_name,
        opt.breakpad_id,
        opt.symbol_directory,
        opt.full,
    ))
}

async fn dump_table(
    debug_name: &str,
    breakpad_id: Option<String>,
    symbol_directory: PathBuf,
    full: bool,
) -> anyhow::Result<()> {
    let table = get_table(debug_name, breakpad_id, symbol_directory).await?;
    println!("Found {} symbols.", table.addr.len());
    for (i, address) in table.addr.iter().enumerate() {
        if i >= 15 && !full {
            println!(
                "and {} more symbols. Pass --full to print the full list.",
                table.addr.len() - i
            );
            break;
        }

        let start_pos = table.index[i];
        let end_pos = table.index[i + 1];
        let symbol_bytes = &table.buffer[start_pos as usize..end_pos as usize];
        let symbol_string = std::str::from_utf8(symbol_bytes)?;
        println!("{:x} {}", address, symbol_string);
    }
    Ok(())
}

async fn get_table(
    debug_name: &str,
    breakpad_id: Option<String>,
    symbol_directory: PathBuf,
) -> anyhow::Result<CompactSymbolTable> {
    let helper = Helper { symbol_directory };
    let table = get_symbols_retry_id(debug_name, breakpad_id, &helper).await?;
    Ok(table)
}

async fn get_symbols_retry_id(
    debug_name: &str,
    breakpad_id: Option<String>,
    helper: &Helper,
) -> anyhow::Result<CompactSymbolTable> {
    let breakpad_id = match breakpad_id {
        Some(breakpad_id) => breakpad_id,
        None => {
            // No breakpad ID was specified. get_compact_symbol_table always wants one, so we call it twice:
            // First, with a bogus breakpad ID ("<unspecified>"), and then again with the breakpad ID that
            // it expected.
            let result = profiler_get_symbols::get_compact_symbol_table(
                debug_name,
                "<unspecified>",
                helper,
            )
            .await;
            match result {
                Ok(table) => return Ok(table),
                Err(err) => match err {
                    GetSymbolsError::UnmatchedBreakpadId(expected, _) => {
                        println!("Using breakpadID: {}", expected);
                        expected
                    }
                    GetSymbolsError::NoMatchMultiArch(errors) => {
                        // There's no one breakpad ID. We need the user to specify which one they want.
                        // Print out all potential breakpad IDs so that the user can pick.
                        let mut potential_ids: Vec<String> = vec![];
                        for err in errors {
                            if let GetSymbolsError::UnmatchedBreakpadId(expected, _) = err {
                                potential_ids.push(expected);
                            } else {
                                return Err(err.into());
                            }
                        }
                        println!("This is a multi-arch container. Please specify one of the following breakpadIDs to pick a symbol table:");
                        for id in potential_ids {
                            println!(" - {}", id);
                        }
                        std::process::exit(0);
                    }
                    err => return Err(err.into()),
                },
            }
        }
    };
    Ok(
        profiler_get_symbols::get_compact_symbol_table(debug_name, &breakpad_id, helper)
            .await?,
    )
}

struct MmapFileContents(memmap::Mmap);

impl OwnedFileData for MmapFileContents {
    fn get_data(&self) -> &[u8] {
        &*self.0
    }
}

struct Helper {
    symbol_directory: PathBuf,
}

impl FileAndPathHelper for Helper {
    type FileContents = MmapFileContents;

    fn get_candidate_paths_for_binary_or_pdb(
        &self,
        debug_name: &str,
        _breakpad_id: &str,
    ) -> Pin<Box<dyn Future<Output = FileAndPathHelperResult<Vec<PathBuf>>>>> {
        async fn to_future(
            res: FileAndPathHelperResult<Vec<PathBuf>>,
        ) -> FileAndPathHelperResult<Vec<PathBuf>> {
            res
        }
        Box::pin(to_future(Ok(vec![self.symbol_directory.join(debug_name)])))
    }

    fn read_file(
        &self,
        path: &Path,
    ) -> Pin<Box<dyn Future<Output = FileAndPathHelperResult<Self::FileContents>>>> {
        async fn read_file_impl(path: PathBuf) -> FileAndPathHelperResult<MmapFileContents> {
            println!("Reading file {:?}", &path);
            let file = File::open(&path)?;
            Ok(MmapFileContents(unsafe { MmapOptions::new().map(&file)? }))
        }

        Box::pin(read_file_impl(path.to_owned()))
    }
}

#[cfg(test)]
mod test {

    use std::path::PathBuf;

    fn fixtures_dir() -> PathBuf {
        let this_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        this_dir.join("..").join("..").join("fixtures")
    }

    #[test]
    fn successful_pdb() {
        let result = futures::executor::block_on(crate::get_table(
            "firefox.pdb",
            Some(String::from("AA152DEB2D9B76084C4C44205044422E2")),
            fixtures_dir().join("win64-ci"),
        ));
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.addr.len(), 1286);
        assert_eq!(result.addr[776], 0x31fc0);
        assert_eq!(
            std::str::from_utf8(
                &result.buffer[result.index[776] as usize..result.index[777] as usize]
            ),
            Ok("sandbox::ProcessMitigationsWin32KDispatcher::EnumDisplayMonitors")
        );
    }
    #[test]
    fn successful_pdb_unspecified_id() {
        let result = futures::executor::block_on(crate::get_table(
            "firefox.pdb",
            None,
            fixtures_dir().join("win64-ci"),
        ));
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.addr.len(), 1286);
        assert_eq!(result.addr[776], 0x31fc0);
        assert_eq!(
            std::str::from_utf8(
                &result.buffer[result.index[776] as usize..result.index[777] as usize]
            ),
            Ok("sandbox::ProcessMitigationsWin32KDispatcher::EnumDisplayMonitors")
        );
    }
}
