//! `fio-ext4` — read and write files inside an ext2/ext3/ext4 image.
//!
//! Everything `debugfs` is usually reached for, without needing root, a mount
//! or a Linux kernel.

use clap::{Parser, Subcommand};

use fio_ext4::Volume;
use mkfs_ext4::device::FileDevice;

#[derive(Parser, Debug)]
#[command(
    name = "fio-ext4",
    about = "Read and write files inside an ext2/ext3/ext4 image",
    version
)]
struct Args {
    /// The image or device to work on.
    image: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List a directory.
    Ls {
        /// Path inside the image.
        #[arg(default_value = "/")]
        path: String,
        /// Show sizes and inode numbers.
        #[arg(short, long)]
        long: bool,
    },
    /// Print a file.
    Cat {
        /// Path inside the image.
        path: String,
    },
    /// Copy a host file into the image.
    Put {
        /// File on this machine.
        source: String,
        /// Destination inside the image.
        dest: String,
    },
    /// Copy a file out of the image.
    Get {
        /// Path inside the image.
        source: String,
        /// Destination on this machine.
        dest: String,
    },
    /// Create a directory, and any parents it needs.
    Mkdir {
        /// Path inside the image.
        path: String,
    },
    /// Remove a file.
    Rm {
        /// Path inside the image.
        path: String,
    },
    /// Remove an empty directory.
    Rmdir {
        /// Path inside the image.
        path: String,
    },
    /// Show what is known about a path.
    Stat {
        /// Path inside the image.
        path: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let device = FileDevice::open(&args.image)
        .await
        .map_err(|e| anyhow::anyhow!("cannot open {}: {e}", args.image))?;
    let mut vol = Volume::open(device).await?;

    match args.command {
        Command::Ls { path, long } => {
            let mut entries = vol.read_dir(&path).await?;
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            for entry in entries {
                if long {
                    let full = if path.ends_with('/') {
                        format!("{path}{}", entry.name)
                    } else {
                        format!("{path}/{}", entry.name)
                    };
                    let st = vol.stat(&full).await?;
                    println!(
                        "{:>7}  {:>8}  {}{}",
                        entry.inode,
                        st.size,
                        entry.name,
                        if entry.is_dir { "/" } else { "" }
                    );
                } else {
                    println!("{}{}", entry.name, if entry.is_dir { "/" } else { "" });
                }
            }
        }

        Command::Cat { path } => {
            let data = vol.read(&path).await?;
            use std::io::Write;
            std::io::stdout().write_all(&data)?;
        }

        Command::Put { source, dest } => {
            let data = std::fs::read(&source)?;
            // Make the parent path if it is not there, so a single command can
            // place a file anywhere.
            if let Some(parent) = dest.rfind('/').filter(|&i| i > 0).map(|i| &dest[..i]) {
                vol.mkdir_all(parent).await?;
            }
            vol.write(&dest, &data).await?;
            vol.flush().await?;
            println!("{} -> {} ({} bytes)", source, dest, data.len());
        }

        Command::Get { source, dest } => {
            let data = vol.read(&source).await?;
            std::fs::write(&dest, &data)?;
            println!("{} -> {} ({} bytes)", source, dest, data.len());
        }

        Command::Mkdir { path } => {
            vol.mkdir_all(&path).await?;
            vol.flush().await?;
        }

        Command::Rm { path } => {
            vol.unlink(&path).await?;
            vol.flush().await?;
        }

        Command::Rmdir { path } => {
            vol.rmdir(&path).await?;
            vol.flush().await?;
        }

        Command::Stat { path } => {
            let st = vol.stat(&path).await?;
            println!("path:   {path}");
            println!("inode:  {}", st.inode);
            println!("size:   {}", st.size);
            println!("mode:   {:o}", st.mode);
            println!("links:  {}", st.links);
            println!("uid:    {}", st.uid);
            println!("gid:    {}", st.gid);
            println!("blocks: {} (512-byte)", st.blocks);
            println!(
                "type:   {}",
                if st.is_dir() {
                    "directory"
                } else if st.is_file() {
                    "regular file"
                } else {
                    "other"
                }
            );
        }
    }

    Ok(())
}
