const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const ICO_DIRECTORY_SIZE: usize = 22;

/// Wraps a validated PNG in an ICO directory entry. Windows accepts PNG payloads in ICO files.
pub(crate) fn png_to_ico(png: &[u8]) -> Result<Vec<u8>, String> {
    if png.len() < 33 || png.get(..8) != Some(PNG_SIGNATURE.as_slice()) {
        return Err("icon.png is not a valid PNG file".to_owned());
    }
    if png.get(12..16) != Some(b"IHDR") || u32::from_be_bytes(png[8..12].try_into().unwrap()) != 13
    {
        return Err("icon.png has an invalid PNG IHDR chunk".to_owned());
    }
    let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
    if width == 0 || height == 0 {
        return Err("icon.png width and height must be greater than zero".to_owned());
    }
    if width > 256 || height > 256 {
        return Err(format!(
            "icon.png dimensions {width}x{height} exceed the Windows icon limit of 256x256"
        ));
    }
    let png_size = u32::try_from(png.len())
        .map_err(|_| "icon.png is too large to store in an ICO file".to_owned())?;

    let mut ico = Vec::with_capacity(ICO_DIRECTORY_SIZE + png.len());
    ico.extend_from_slice(&[0, 0, 1, 0, 1, 0]);
    ico.push(if width == 256 { 0 } else { width as u8 });
    ico.push(if height == 256 { 0 } else { height as u8 });
    ico.extend_from_slice(&[0, 0]);
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&32u16.to_le_bytes());
    ico.extend_from_slice(&png_size.to_le_bytes());
    ico.extend_from_slice(&(ICO_DIRECTORY_SIZE as u32).to_le_bytes());
    ico.extend_from_slice(png);
    Ok(ico)
}

#[cfg(all(test, windows))]
mod tests {
    use super::png_to_ico;

    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&width.to_be_bytes());
        png.extend_from_slice(&height.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        png.extend_from_slice(&[0, 0, 0, 0]);
        png
    }

    #[test]
    fn creates_png_payload_ico_with_valid_header_and_256_dimensions() {
        let png = png_header(256, 128);
        let ico = png_to_ico(&png).unwrap();

        assert_eq!(&ico[..6], &[0, 0, 1, 0, 1, 0]);
        assert_eq!(&ico[6..10], &[0, 128, 0, 0]);
        assert_eq!(u16::from_le_bytes([ico[10], ico[11]]), 1);
        assert_eq!(u16::from_le_bytes([ico[12], ico[13]]), 32);
        assert_eq!(
            u32::from_le_bytes(ico[14..18].try_into().unwrap()),
            png.len() as u32
        );
        assert_eq!(u32::from_le_bytes(ico[18..22].try_into().unwrap()), 22);
        assert_eq!(&ico[22..], png);
    }

    #[test]
    fn rejects_icons_larger_than_256_or_invalid_png_headers() {
        assert!(
            png_to_ico(&png_header(257, 1))
                .unwrap_err()
                .contains("256x256")
        );
        assert!(png_to_ico(b"not a png").is_err());
        assert!(png_to_ico(&png_header(0, 10)).is_err());
    }
}
