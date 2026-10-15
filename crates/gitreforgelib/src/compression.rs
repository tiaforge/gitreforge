use std::{
    error::Error,
    fs::File,
    io::{self, BufWriter, Write},
    path::Path,
};

use flate2::Status;
use libdeflater::Decompressor;
use memmap2::Mmap;

use crate::{WriteBytes, packreader::PackObject};

pub struct Decompression {
    libdeflate_decompressor: Box<Decompressor>,
    flate2_decompressor: Box<flate2::Decompress>,
}

impl Default for Decompression {
    fn default() -> Self {
        Self {
            libdeflate_decompressor: Box::new(Decompressor::new()),
            flate2_decompressor: Box::new(flate2::Decompress::new(false)),
        }
    }
}

pub fn pack_file(path: &Path, prefix: &str, write_bytes: &WriteBytes) -> Result<(), io::Error> {
    let file = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?;
    let data = &write_bytes.bytes[write_bytes.start..];
    let mut buf_writer = BufWriter::new(file);
    let preamble: Vec<_> = format!("{} {}\0", prefix, data.len()).bytes().collect();

    if std::env::var_os("GITREFORGE_LOOSE_LIBDEFLATE").is_some() {
        let mut input = preamble.clone();
        input.extend_from_slice(data);
        let mut c = libdeflater::Compressor::new(crate::pack_writer::compression_level());
        let mut out = vec![0; c.zlib_compress_bound(input.len())];
        let n = c.zlib_compress(&input, &mut out).unwrap();
        buf_writer.write_all(&out[..n]).unwrap();
        return Ok(());
    }

    let mut compress = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    compress.write_all(&preamble).unwrap();
    compress.write_all(data).unwrap();
    let data = compress.finish().unwrap();

    buf_writer.write_all(&data).unwrap();

    Ok(())
}

impl Decompression {
    #[allow(clippy::uninit_vec)]
    pub fn unpack(
        &mut self,
        mmap: &Mmap,
        pack_object: &PackObject,
        additional_offset: usize,
    ) -> Box<[u8]> {
        let slice = &mmap[pack_object.offset + pack_object.header_len + additional_offset + 2..];

        let mut buf: Vec<u8> = Vec::with_capacity(pack_object.data_size);
        unsafe { buf.set_len(pack_object.data_size) };

        self.libdeflate_decompressor
            .deflate_decompress(slice, &mut buf)
            .unwrap();

        buf.into_boxed_slice()
    }

    pub fn unpack_file(
        &mut self,
        base_path: &Path,
        hash_code: &str,
    ) -> Result<Box<[u8]>, Box<dyn Error>> {
        let (x, xs) = hash_code.split_at(2);
        let file_path = base_path.join("objects").join(x).join(xs);

        let compressed = std::fs::read(file_path)?;
        // skip the zlib header, the decompressor reads raw deflate
        let input = compressed.get(2..).ok_or("loose object is too short")?;

        self.flate2_decompressor.reset(false);
        let mut output_buf = Vec::with_capacity(input.len() * 4);

        loop {
            if output_buf.capacity() - output_buf.len() < 8192 {
                output_buf.reserve(output_buf.capacity().max(8192));
            }

            let total_in = self.flate2_decompressor.total_in();
            let total_out = self.flate2_decompressor.total_out();
            let status = self.flate2_decompressor.decompress_vec(
                &input[total_in as usize..],
                &mut output_buf,
                flate2::FlushDecompress::None,
            )?;

            if status == Status::StreamEnd {
                break;
            }
            if self.flate2_decompressor.total_in() == total_in
                && self.flate2_decompressor.total_out() == total_out
            {
                return Err("loose object is truncated or corrupt".into());
            }
        }

        Ok(output_buf.into_boxed_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(miri, ignore)]
    fn unpack_file_reads_highly_compressible_object_completely() {
        let repo = std::env::temp_dir().join(format!(
            "gitreforge-unpack-file-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let hash = "0123456789abcdef0123456789abcdef01234567";
        std::fs::create_dir_all(repo.join("objects").join(&hash[..2])).unwrap();

        // compresses far better than 1:2, so the inflated data outgrows
        // what a single read chunk reserves
        let data: Vec<u8> = b"Signed-off-by: A User <a@example.com>\n".repeat(2000);
        let write_bytes = WriteBytes {
            bytes: data.clone().into_boxed_slice(),
            start: 0,
        };
        pack_file(
            &repo.join("objects").join(&hash[..2]).join(&hash[2..]),
            "commit",
            &write_bytes,
        )
        .unwrap();

        let result = Decompression::default().unpack_file(&repo, hash);
        std::fs::remove_dir_all(&repo).unwrap();

        let mut expected = format!("commit {}\0", data.len()).into_bytes();
        expected.extend_from_slice(&data);
        assert_eq!(result.unwrap().len(), expected.len());
    }
}
