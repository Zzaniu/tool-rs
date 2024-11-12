use std::io::{Read, Seek, Write};

use zip::ZipWriter;

use anyhow::Result;

pub struct InnerZipFileInfo {
    pub file_name: String,
    pub file_content: Option<Vec<u8>>,
}

impl InnerZipFileInfo {
    pub fn new(file_name: String, file_content: Option<Vec<u8>>) -> Self {
        Self {
            file_name,
            file_content,
        }
    }
}

pub struct ZipFileInfo {
    pub inner: Vec<InnerZipFileInfo>,
}

impl From<InnerZipFileInfo> for ZipFileInfo {
    fn from(value: InnerZipFileInfo) -> Self {
        Self { inner: vec![value] }
    }
}

impl ZipFileInfo {
    pub fn new(inner: Vec<InnerZipFileInfo>) -> Self {
        Self { inner }
    }

    /// 压缩文件
    pub fn zip_file_bytes(self) -> Result<Vec<u8>> {
        let mut archive = std::io::Cursor::new(Vec::with_capacity(102400));
        {
            let zip_writer = ZipWriter::new(&mut archive);
            self.push_zip_file(zip_writer)?;
        }
        Ok(archive.into_inner())
    }

    /// 往已有zip文件中添加文件
    #[allow(unused)]
    pub fn zip_append_file(self, exists_zip_file_path: impl AsRef<std::path::Path>) -> Result<()> {
        let zip_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(exists_zip_file_path.as_ref())?;
        let zip_writer = ZipWriter::new_append(zip_file)?;
        self.push_zip_file(zip_writer)?;
        Ok(())
    }

    /// 往已有zip文件中添加文件
    pub fn zip_append_file_bytes(self, exists_zip_file_content: &mut Vec<u8>) -> Result<()> {
        let mut zip_file = std::io::Cursor::new(exists_zip_file_content);
        let zip_writer = ZipWriter::new_append(&mut zip_file)?;
        self.push_zip_file(zip_writer)?;
        Ok(())
    }

    fn push_zip_file<W>(self, mut zip_writer: ZipWriter<W>) -> Result<()>
    where
        W: Read + Write + Seek,
    {
        let file_options = zip::write::SimpleFileOptions::default();
        for mut item in self.inner {
            zip_writer.start_file(item.file_name.as_str(), file_options)?;
            if item.file_content.is_some() {
                zip_writer.write_all(item.file_content.take().as_deref().unwrap())?;
            } else {
                let mut f = std::fs::File::open(item.file_name)?;
                std::io::copy(&mut f, &mut zip_writer)?;
            }
        }
        zip_writer.finish()?;
        Ok(())
    }
}
