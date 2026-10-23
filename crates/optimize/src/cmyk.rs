//! CMYK images: resampling and JPEG recompression for print PDFs.
//!
//! Book and magazine PDFs made for print keep their pictures in CMYK, often as barely compressed
//! Flate streams at 300 ppi and more. The grey/RGB path in [`crate::images`] leaves them alone, so
//! Reduce File Size could hardly shrink such a file (a 709 MB book went down by a few MB, where
//! Acrobat makes 35 MB of it). This module handles them without converting any colour:
//!
//! - The images stay CMYK, in their own colour space (DeviceCMYK or an ICC profile with four
//!   components), with their own decode array.
//! - **The stored sample values are kept as they are.** Four-component JPEGs carry a convention
//!   viewers disagree on: Adobe's writers store inverted values and mark the file with an `APP14
//!   Adobe` segment, and some viewers invert such JPEGs back, others (following the PDF
//!   specification, as PdfCraft's renderer does) read the stored values as they are. A
//!   recompressed image therefore stores the same values its original stored, and carries the
//!   Adobe segment exactly when its original did (a Flate original: never). Whichever way a viewer
//!   reads four-component JPEGs, it reads the new image the way it read the old one.
//! - YCCK JPEGs (Adobe transform 2), images with masks of any kind, other bit depths and images
//!   too large to decode are left alone.

use pdfcraft_cos::{Dict, Document, Object, Stream};

use crate::{Compression, ImageSettings};

/// Images with more pixels than this are left alone (memory: four bytes a pixel).
const MAX_PIXELS: u64 = 60_000_000;

/// Whether `cs` is a four-component (CMYK) colour space: DeviceCMYK, or an ICC profile with four
/// components.
pub(crate) fn is_cmyk(doc: &Document, cs: &Object) -> bool {
    match &*doc.resolve(cs) {
        Object::Name(n) => matches!(n.as_slice(), b"DeviceCMYK" | b"CMYK"),
        Object::Array(a) => {
            a.first().and_then(Object::as_name) == Some(b"ICCBased".as_slice())
                && a.get(1).is_some_and(|p| matches!(&*doc.resolve(p), Object::Stream(icc) if icc.dict.int(b"N") == Some(4)))
        }
        _ => false,
    }
}

/// The image's single filter, if it has exactly one (a name, or an array of one name).
fn single_filter(s: &Stream) -> Option<Option<Vec<u8>>> {
    match s.dict.get(b"Filter") {
        None => Some(None),
        Some(Object::Name(n)) => Some(Some(n.clone())),
        Some(Object::Array(a)) if a.len() == 1 => Some(Some(a[0].as_name()?.to_vec())),
        Some(Object::Array(a)) if a.is_empty() => Some(None),
        _ => None,
    }
}

/// The JPEG's segments before its scan: (offset of the marker, marker, total length with the
/// marker). Stops at the first byte that isn't a marker, and at the scan or end of the data.
fn segments(jpeg: &[u8]) -> Vec<(usize, u8, usize)> {
    let mut out = Vec::new();
    let mut i = 2usize;
    while let (Some(0xFF), Some(&marker)) = (jpeg.get(i), jpeg.get(i + 1)) {
        if marker == 0xDA || marker == 0xD9 {
            break;
        }
        let Some(&[hi, lo]) = jpeg.get(i + 2..i + 4).and_then(|b| <&[u8; 2]>::try_from(b).ok()) else { break };
        let total = 2 + usize::from(u16::from_be_bytes([hi, lo]));
        if jpeg.len() < i.saturating_add(total) {
            break;
        }
        out.push((i, marker, total));
        i += total;
    }
    out
}

/// Whether the segment at `at` is an `APP14 Adobe` one.
fn is_adobe(jpeg: &[u8], at: usize, marker: u8) -> bool {
    marker == 0xEE && jpeg.get(at + 4..at + 9) == Some(b"Adobe".as_slice())
}

/// What a JPEG's `APP14 Adobe` segment says: `None` without one, else its colour transform
/// (0: none, CMYK; 2: YCCK).
pub(crate) fn adobe_transform(jpeg: &[u8]) -> Option<u8> {
    let (at, _, _) = segments(jpeg).into_iter().find(|&(at, m, _)| is_adobe(jpeg, at, m))?;
    jpeg.get(at + 15).copied()
}

/// The JPEG without its `APP14 Adobe` segment.
fn strip_adobe(mut jpeg: Vec<u8>) -> Vec<u8> {
    if let Some((at, _, total)) = segments(&jpeg).into_iter().find(|&(at, m, _)| is_adobe(&jpeg, at, m)) {
        // `segments` checked that the segment lies within the data.
        jpeg.drain(at..at + total);
    }
    jpeg
}

/// The stored samples of a four-component JPEG, as they are in the file (no inversion, no
/// colour transform), and whether it has an Adobe segment. YCCK JPEGs give `None`.
fn jpeg_samples(data: &[u8], w: u32, h: u32) -> Option<(Vec<u8>, bool)> {
    use zune_jpeg::zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};
    let adobe = adobe_transform(data);
    if adobe.is_some_and(|t| t != 0) {
        return None;
    }
    let options = DecoderOptions::default().set_max_width(usize::from(u16::MAX)).set_max_height(usize::from(u16::MAX));
    let mut dec = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(data), options);
    dec.decode_headers().ok()?;
    if dec.input_colorspace()? != ColorSpace::CMYK || dec.dimensions()? != (w as usize, h as usize) {
        return None;
    }
    dec.set_options(options.jpeg_set_out_colorspace(ColorSpace::CMYK));
    let px = dec.decode().ok()?;
    let expected = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    (px.len() == expected).then_some((px, adobe.is_some()))
}

/// A four-component JPEG that stores exactly `px`, with an Adobe segment (transform 0) or none.
pub(crate) fn encode_jpeg(px: &[u8], w: u32, h: u32, quality: u8, adobe: bool) -> Option<Vec<u8>> {
    let (w16, h16) = (u16::try_from(w).ok()?, u16::try_from(h).ok()?);
    // jpeg-encoder writes CMYK the Adobe way: inverted, with the Adobe segment. Inverting first
    // makes it store `px` itself.
    let inverted: Vec<u8> = px.iter().map(|v| 255 - v).collect();
    let mut out = Vec::new();
    let mut enc = jpeg_encoder::Encoder::new(&mut out, quality.clamp(1, 100));
    // Every component at full resolution. Below quality 90 jpeg-encoder subsamples components 2
    // and 3 (the chroma of YCbCr); for CMYK those are magenta and yellow, and such JPEGs came back
    // with cyan, magenta and yellow off by 50 to 100 levels.
    enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
    enc.encode(&inverted, w16, h16, jpeg_encoder::ColorType::Cmyk).ok()?;
    Some(if adobe { out } else { strip_adobe(out) })
}

fn resample(px: Vec<u8>, w: u32, h: u32, nw: u32, nh: u32) -> Option<Vec<u8>> {
    use image::imageops::{FilterType, resize};
    // Four independent channels: an RGBA buffer filters each one on its own, unweighted.
    Some(resize(&image::RgbaImage::from_raw(w, h, px)?, nw, nh, FilterType::CatmullRom).into_raw())
}

/// One CMYK image: resample and/or recompress per `settings`; `ppi` is its effective resolution.
/// The new image, if it is smaller.
pub(crate) fn process(s: &Stream, ppi: f64, settings: &ImageSettings) -> Option<Stream> {
    let d = &s.dict;
    if d.contains(b"SMask") || d.contains(b"Mask") || matches!(d.get(b"ImageMask"), Some(Object::Bool(true))) {
        return None;
    }
    if d.int(b"BitsPerComponent") != Some(8) {
        return None;
    }
    let (w, h) = (u32::try_from(d.int(b"Width")?).ok()?, u32::try_from(d.int(b"Height")?).ok()?);
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return None;
    }
    let was_dct = match single_filter(s)? {
        Some(f) if matches!(f.as_slice(), b"DCTDecode" | b"DCT") => true,
        Some(f) if matches!(f.as_slice(), b"FlateDecode" | b"Fl" | b"LZWDecode" | b"LZW" | b"RunLengthDecode" | b"RL") => false,
        None => false,
        Some(_) => return None,
    };
    let resize = settings.downsample && ppi.is_finite() && ppi > settings.above_ppi && settings.target_ppi > 0.0;
    let recompress = matches!(settings.compression, Compression::Jpeg(_)) || (matches!(settings.compression, Compression::Flate) && was_dct);
    if !resize && !recompress {
        return None;
    }
    if !resize && u64::from(w) * u64::from(h) < 64 * 64 {
        return None;
    }
    let (nw, nh) = if resize {
        let k = settings.target_ppi / ppi;
        (((f64::from(w) * k).round() as u32).max(1), ((f64::from(h) * k).round() as u32).max(1))
    } else {
        (w, h)
    };
    let (px, adobe) = if was_dct {
        jpeg_samples(&s.raw, w, h)?
    } else {
        // Decoding stops a little past the image's own size (`MAX_PIXELS` caps it), so a stream
        // that inflates to far more than its dictionary says can't take the memory.
        let n = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
        let v = s.decoded_within(n.saturating_add(1 << 16)).ok()?;
        (v.get(..n)?.to_vec(), false)
    };
    let px = if (nw, nh) != (w, h) { resample(px, w, h, nw, nh)? } else { px };
    let mut nd: Dict = d.clone();
    for k in [&b"Filter"[..], b"DecodeParms", b"Length", b"DL"] {
        nd.remove(k);
    }
    nd.set(b"Width".to_vec(), Object::Int(i64::from(nw)));
    nd.set(b"Height".to_vec(), Object::Int(i64::from(nh)));
    let quality = match settings.compression {
        Compression::Jpeg(q) => Some(q),
        Compression::Retain if was_dct => Some(80),
        _ => None,
    };
    let new = match quality {
        Some(q) => {
            let data = encode_jpeg(&px, nw, nh, q, adobe)?;
            nd.set(b"Filter".to_vec(), Object::name("DCTDecode"));
            Stream::from_raw(nd, data)
        }
        None => Stream::flate(nd, &px),
    };
    (new.raw.len() < s.raw.len()).then_some(new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ImageSettings;

    fn gradient(w: u32, h: u32) -> Vec<u8> {
        let mut px = Vec::new();
        for y in 0..h {
            for x in 0..w {
                px.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, 180, 20]);
            }
        }
        px
    }

    fn stored(jpeg: &[u8], w: u32, h: u32) -> Vec<u8> {
        jpeg_samples(jpeg, w, h).unwrap().0
    }

    fn mean_diff(a: &[u8], b: &[u8]) -> f64 {
        a.iter().zip(b).map(|(x, y)| (f64::from(*x) - f64::from(*y)).abs()).sum::<f64>() / a.len() as f64
    }

    fn cmyk_dict(w: u32, h: u32) -> Dict {
        let mut d = Dict::new();
        d.set(b"Type".to_vec(), Object::name("XObject"));
        d.set(b"Subtype".to_vec(), Object::name("Image"));
        d.set(b"Width".to_vec(), Object::Int(i64::from(w)));
        d.set(b"Height".to_vec(), Object::Int(i64::from(h)));
        d.set(b"ColorSpace".to_vec(), Object::name("DeviceCMYK"));
        d.set(b"BitsPerComponent".to_vec(), Object::Int(8));
        d
    }

    #[test]
    fn encoded_jpegs_store_the_samples_themselves() {
        let px = gradient(64, 48);
        for (adobe, quality) in [(false, 95), (true, 95), (false, 60), (true, 25)] {
            let j = encode_jpeg(&px, 64, 48, quality, adobe).unwrap();
            assert_eq!(adobe_transform(&j).is_some(), adobe);
            let diff = mean_diff(&stored(&j, 64, 48), &px);
            assert!(diff < 3.0, "stored values changed by {diff} (adobe: {adobe}, quality {quality})");
        }
    }

    #[test]
    fn a_flate_cmyk_image_is_downsampled_to_a_jpeg_without_an_adobe_segment() {
        let (w, h) = (600, 400);
        let px = gradient(w, h);
        let s = Stream::flate(cmyk_dict(w, h), &px);
        // Drawn at 600 ppi: Reduce File Size takes it to 150.
        let new = process(&s, 600.0, &ImageSettings::REDUCE).unwrap();
        assert_eq!((new.dict.int(b"Width"), new.dict.int(b"Height")), (Some(150), Some(100)));
        assert_eq!(new.dict.name(b"Filter"), Some(b"DCTDecode".as_slice()));
        assert_eq!(new.dict.name(b"ColorSpace"), Some(b"DeviceCMYK".as_slice()), "the colour space is kept");
        assert_eq!(adobe_transform(&new.raw), None);
        let small = resample(px, w, h, 150, 100).unwrap();
        let diff = mean_diff(&stored(&new.raw, 150, 100), &small);
        assert!(diff < 4.0, "diff {diff}");
        assert!(new.raw.len() * 10 < s.raw.len());
    }

    #[test]
    fn an_adobe_cmyk_jpeg_keeps_its_convention() {
        let (w, h) = (400, 400);
        let px = gradient(w, h);
        let mut d = cmyk_dict(w, h);
        d.set(b"Filter".to_vec(), Object::name("DCTDecode"));
        let s = Stream::from_raw(d, encode_jpeg(&px, w, h, 95, true).unwrap());
        let new = process(&s, 400.0, &ImageSettings::REDUCE).unwrap();
        assert_eq!(adobe_transform(&new.raw), Some(0), "the Adobe segment stays");
        let before = resample(stored(&s.raw, w, h), w, h, 150, 150).unwrap();
        let diff = mean_diff(&stored(&new.raw, 150, 150), &before);
        assert!(diff < 4.0, "the stored values changed: {diff}");
    }

    #[test]
    fn masked_and_ycck_images_are_left_alone() {
        let (w, h) = (300, 300);
        let mut d = cmyk_dict(w, h);
        d.set(b"Mask".to_vec(), Object::Array(vec![Object::Int(0), Object::Int(0)]));
        assert!(process(&Stream::flate(d, &gradient(w, h)), 600.0, &ImageSettings::REDUCE).is_none());
        let mut ycck = Vec::new();
        jpeg_encoder::Encoder::new(&mut ycck, 90).encode(&gradient(w, h), 300, 300, jpeg_encoder::ColorType::CmykAsYcck).unwrap();
        assert_eq!(adobe_transform(&ycck), Some(2));
        let mut d = cmyk_dict(w, h);
        d.set(b"Filter".to_vec(), Object::name("DCTDecode"));
        assert!(process(&Stream::from_raw(d, ycck), 600.0, &ImageSettings::REDUCE).is_none());
    }

    #[test]
    fn icc_cmyk_counts_as_cmyk() {
        let mut doc = Document::new_empty();
        let mut icc = Dict::new();
        icc.set(b"N".to_vec(), Object::Int(4));
        let r = doc.add(Object::Stream(Stream::from_raw(icc, Vec::new())));
        assert!(is_cmyk(&doc, &Object::Array(vec![Object::name("ICCBased"), Object::Ref(r)])));
        assert!(is_cmyk(&doc, &Object::name("DeviceCMYK")));
        assert!(!is_cmyk(&doc, &Object::name("DeviceRGB")));
    }
}
