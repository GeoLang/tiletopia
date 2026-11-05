use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use terrano_core::SampleFormat;

use crate::imagery::PixelGrid;

#[cfg(feature = "jp2")]
pub(crate) use grok::{Jp2Layout, decode_rows, read_layout};

const MAX_WINDOW_BYTES: u64 = 64 << 20;

// grok decodes every sample into 32 bits at most
const GROK_SAMPLE_BYTES: u64 = 4;

pub(crate) fn rows_per_window(
    cols: usize,
    samples: usize,
    tile_height: usize,
) -> Result<usize, String> {
    let row_bytes = cols as u64 * samples as u64 * GROK_SAMPLE_BYTES;
    let rows_in_budget = (MAX_WINDOW_BYTES / row_bytes) as usize;
    if rows_in_budget == 0 {
        return Err(format!(
            "one row of {cols} pixels and {samples} bands takes {row_bytes} bytes to decode, \
             past the {MAX_WINDOW_BYTES} byte limit"
        ));
    }
    // windows that end inside a tile row decode that row twice
    let whole_tile_rows = rows_in_budget / tile_height * tile_height;
    Ok(if whole_tile_rows > 0 {
        whole_tile_rows
    } else {
        rows_in_budget
    })
}

pub(crate) fn sample_format(precision: u8, signed: bool) -> Result<SampleFormat, String> {
    match (signed, precision) {
        (false, 8) => Ok(SampleFormat::U8),
        (false, 1..=16) => Ok(SampleFormat::U16),
        (true, 1..=16) => Ok(SampleFormat::I16),
        _ => Err(format!(
            "{precision} bit JPEG 2000 samples are not served, the limit is 16"
        )),
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct GmlPlacement {
    pub(crate) grid: PixelGrid,
    pub(crate) epsg: Option<u32>,
}

fn srs_name_epsg(element: &BytesStart) -> Option<u32> {
    let attribute = element
        .attributes()
        .flatten()
        .find(|attribute| attribute.key.local_name().as_ref() == b"srsName")?;
    let srs_name = std::str::from_utf8(&attribute.value).ok()?;
    if !srs_name.to_ascii_lowercase().contains("epsg") {
        return None;
    }
    srs_name
        .rsplit(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

fn gml_numbers(text: &str) -> Option<Vec<f64>> {
    text.split(|c: char| c.is_whitespace() || c == ',')
        .filter(|part| !part.is_empty())
        .map(|part| part.parse().ok())
        .collect()
}

enum GmlNumbers {
    Origin,
    OffsetVector,
}

pub(crate) fn gml_placement(xml: &[u8]) -> Option<GmlPlacement> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut inside_grid = false;
    let mut inside_origin = false;
    let mut numbers_element = None;
    let mut origin = None;
    let mut offset_vectors = Vec::new();
    let mut grid_epsg = None;
    let mut document_epsg = None;
    loop {
        match reader.read_event().ok()? {
            Event::Start(element) => {
                match element.local_name().as_ref() {
                    b"RectifiedGrid" => inside_grid = true,
                    b"origin" => inside_origin = inside_grid,
                    b"pos" | b"coordinates" if inside_origin => {
                        numbers_element = Some(GmlNumbers::Origin);
                    }
                    b"offsetVector" if inside_grid => {
                        numbers_element = Some(GmlNumbers::OffsetVector);
                    }
                    _ => {}
                }
                let epsg = srs_name_epsg(&element);
                document_epsg = document_epsg.or(epsg);
                if inside_grid {
                    grid_epsg = grid_epsg.or(epsg);
                }
            }
            Event::Text(text) => match numbers_element.take() {
                Some(GmlNumbers::Origin) => origin = gml_numbers(&text.decode().ok()?),
                Some(GmlNumbers::OffsetVector) => {
                    offset_vectors.push(gml_numbers(&text.decode().ok()?)?);
                }
                None => {}
            },
            Event::End(element) => {
                numbers_element = None;
                match element.local_name().as_ref() {
                    b"origin" => inside_origin = false,
                    b"RectifiedGrid" => break,
                    _ => {}
                }
            }
            Event::Eof => return None,
            _ => {}
        }
    }
    let origin = origin?;
    let [first, second] = origin[..] else {
        return None;
    };
    let [column_step, row_step] = &offset_vectors[..] else {
        return None;
    };
    let [column_first, column_second] = column_step[..] else {
        return None;
    };
    let [row_first, row_second] = row_step[..] else {
        return None;
    };
    // gmljp2 files write either the crs axis order or x then y
    let is_x_first = column_second == 0.0 && row_first == 0.0;
    let is_y_first = column_first == 0.0 && row_second == 0.0;
    let (center_x, center_y, pixel_width, pixel_height) = if is_x_first {
        (first, second, column_first, -row_second)
    } else if is_y_first {
        (second, first, column_second, -row_first)
    } else {
        return None;
    };
    // gmljp2 origins are the center of the top left pixel
    Some(GmlPlacement {
        grid: PixelGrid {
            origin_x: center_x - pixel_width / 2.0,
            origin_y: center_y + pixel_height / 2.0,
            pixel_width,
            pixel_height,
        },
        epsg: grid_epsg.or(document_epsg),
    })
}

#[cfg(feature = "jp2")]
mod grok {
    use std::ffi::{CStr, c_char, c_void};
    use std::ops::Range;
    use std::path::Path;
    use std::ptr;
    use std::sync::Once;

    use grokj2k_sys::{
        _GRK_CHANNEL_TYPE_GRK_CHANNEL_TYPE_OPACITY,
        _GRK_CHANNEL_TYPE_GRK_CHANNEL_TYPE_PREMULTIPLIED_OPACITY, _GRK_COLOR_SPACE_GRK_CLRSPC_CMYK,
        _GRK_COLOR_SPACE_GRK_CLRSPC_CUSTOM_CIE, _GRK_COLOR_SPACE_GRK_CLRSPC_DEFAULT_CIE,
        _GRK_COLOR_SPACE_GRK_CLRSPC_EYCC, _GRK_COLOR_SPACE_GRK_CLRSPC_SYCC,
        _grk_data_type_GRK_INT_16, _grk_data_type_GRK_INT_32, GRK_COLOR_SPACE, grk_decompress,
        grk_decompress_get_image, grk_decompress_init, grk_decompress_parameters,
        grk_decompress_read_header, grk_header_info, grk_image, grk_image_comp, grk_initialize,
        grk_msg_handlers, grk_object, grk_object_unref, grk_set_msg_handlers, grk_stream_params,
    };
    use terrano_core::SampleFormat;

    pub(crate) struct Jp2Layout {
        pub(crate) cols: usize,
        pub(crate) rows: usize,
        pub(crate) samples: usize,
        pub(crate) precision: u8,
        pub(crate) signed: bool,
        pub(crate) tile_height: usize,
        pub(crate) first_opacity_band: Option<usize>,
        pub(crate) geotiff: Option<Vec<u8>>,
        pub(crate) gml_documents: Vec<Vec<u8>>,
    }

    unsafe extern "C" fn log_grok_error(message: *const c_char, _client_data: *mut c_void) {
        let message = unsafe { CStr::from_ptr(message) }.to_string_lossy();
        tracing::warn!("grok: {message}");
    }

    static INITIALIZE_GROK: Once = Once::new();

    fn initialize_grok() {
        INITIALIZE_GROK.call_once(|| unsafe {
            grk_initialize(ptr::null(), 0, ptr::null_mut());
            // grk_initialize overwrites handlers set before it
            let mut handlers: grk_msg_handlers = std::mem::zeroed();
            handlers.error_callback = Some(log_grok_error);
            grk_set_msg_handlers(handlers);
        });
    }

    struct Codec(*mut grk_object);

    impl Drop for Codec {
        fn drop(&mut self) {
            unsafe { grk_object_unref(self.0) };
        }
    }

    fn open_codec(
        path: &Path,
        parameters: &mut grk_decompress_parameters,
        header: &mut grk_header_info,
    ) -> Result<Codec, String> {
        initialize_grok();
        let path_text = path
            .to_str()
            .ok_or_else(|| format!("{}: the path is not UTF-8", path.display()))?;
        let mut stream: grk_stream_params = unsafe { std::mem::zeroed() };
        if path_text.len() >= stream.file.len() {
            return Err(format!(
                "{path_text}: the path is past Grok's limit of {} bytes",
                stream.file.len() - 1
            ));
        }
        for (slot, byte) in stream.file.iter_mut().zip(path_text.bytes()) {
            *slot = byte as c_char;
        }
        let codec = unsafe { grk_decompress_init(&mut stream, parameters) };
        if codec.is_null() {
            return Err("not a readable JPEG 2000 file".into());
        }
        let codec = Codec(codec);
        if !unsafe { grk_decompress_read_header(codec.0, header) } {
            return Err("not a readable JPEG 2000 file: its header cannot be read".into());
        }
        Ok(codec)
    }

    fn codec_image(codec: &Codec) -> Result<&grk_image, String> {
        let image = unsafe { grk_decompress_get_image(codec.0) };
        // grok's composite image lasts until the codec is released
        unsafe { image.as_ref() }.ok_or_else(|| "Grok returned no image".to_string())
    }

    fn image_components(image: &grk_image) -> Result<&[grk_image_comp], String> {
        if image.comps.is_null() || image.numcomps == 0 {
            return Err("this JPEG 2000 image has no components".into());
        }
        Ok(unsafe { std::slice::from_raw_parts(image.comps, usize::from(image.numcomps)) })
    }

    fn unserved_colour_space(colour_space: GRK_COLOR_SPACE) -> Option<&'static str> {
        match colour_space {
            _GRK_COLOR_SPACE_GRK_CLRSPC_SYCC => Some("sYCC"),
            _GRK_COLOR_SPACE_GRK_CLRSPC_EYCC => Some("eYCC"),
            _GRK_COLOR_SPACE_GRK_CLRSPC_CMYK => Some("CMYK"),
            _GRK_COLOR_SPACE_GRK_CLRSPC_DEFAULT_CIE | _GRK_COLOR_SPACE_GRK_CLRSPC_CUSTOM_CIE => {
                Some("CIE Lab")
            }
            _ => None,
        }
    }

    fn check_decodes_to_bands(
        image: &grk_image,
        components: &[grk_image_comp],
    ) -> Result<(), String> {
        const CONVERT: &str = "convert it with gdal_translate -of COG -co COMPRESS=DEFLATE";
        if let Some(name) = unserved_colour_space(image.color_space) {
            return Err(format!(
                "the {name} colour space of this JPEG 2000 file is not served: {CONVERT}"
            ));
        }
        let meta = unsafe { image.meta.as_ref() };
        if meta.is_some_and(|meta| !meta.color.palette.is_null()) {
            return Err(format!(
                "JPEG 2000 palettes are not served: {CONVERT} -expand rgb"
            ));
        }
        if components.iter().any(|c| c.dx != 1 || c.dy != 1) {
            return Err(format!(
                "subsampled JPEG 2000 components are not served: {CONVERT}"
            ));
        }
        let first = &components[0];
        let is_uniform = components
            .iter()
            .all(|c| c.prec == first.prec && c.sgnd == first.sgnd);
        if !is_uniform {
            return Err(format!(
                "JPEG 2000 components of different sample types are not served: {CONVERT}"
            ));
        }
        Ok(())
    }

    pub(crate) fn read_layout(path: &Path) -> Result<Jp2Layout, String> {
        let mut parameters: grk_decompress_parameters = unsafe { std::mem::zeroed() };
        let mut header: grk_header_info = unsafe { std::mem::zeroed() };
        let codec = open_codec(path, &mut parameters, &mut header)?;
        let image = codec_image(&codec)?;
        let components = image_components(image)?;
        check_decodes_to_bands(image, components)?;

        let cols = (image.x1 - image.x0) as usize;
        let rows = (image.y1 - image.y0) as usize;
        if cols == 0 || rows == 0 || header.t_height == 0 {
            return Err(format!(
                "this JPEG 2000 file has {cols} x {rows} pixels in tiles {} rows tall",
                header.t_height
            ));
        }
        let first_opacity_band = components.iter().position(|c| {
            matches!(
                c.type_,
                _GRK_CHANNEL_TYPE_GRK_CHANNEL_TYPE_OPACITY
                    | _GRK_CHANNEL_TYPE_GRK_CHANNEL_TYPE_PREMULTIPLIED_OPACITY
            )
        });
        let geotiff = unsafe { image.meta.as_ref() }
            .filter(|meta| !meta.geotiff_buf.is_null() && meta.geotiff_len > 0)
            .map(|meta| {
                unsafe { std::slice::from_raw_parts(meta.geotiff_buf, meta.geotiff_len) }.to_vec()
            });
        let gml_documents = header.asocs[..header.num_asocs as usize]
            .iter()
            .filter(|asoc| !asoc.xml.is_null() && asoc.xml_len > 0)
            .map(|asoc| {
                unsafe { std::slice::from_raw_parts(asoc.xml, asoc.xml_len as usize) }.to_vec()
            })
            .collect();
        Ok(Jp2Layout {
            cols,
            rows,
            samples: components.len(),
            precision: components[0].prec,
            signed: components[0].sgnd,
            tile_height: header.t_height as usize,
            first_opacity_band,
            geotiff,
            gml_documents,
        })
    }

    unsafe fn sample_at(component: &grk_image_comp, row: usize, col: usize) -> f64 {
        let at = row * component.stride as usize + col;
        unsafe {
            if component.data_type == _grk_data_type_GRK_INT_16 {
                f64::from(component.data.cast::<i16>().add(at).read())
            } else {
                f64::from(component.data.cast::<i32>().add(at).read())
            }
        }
    }

    pub(crate) fn decode_rows(
        path: &Path,
        layout: &Jp2Layout,
        window_rows: Range<usize>,
        format: SampleFormat,
    ) -> Result<Vec<u8>, String> {
        let mut parameters: grk_decompress_parameters = unsafe { std::mem::zeroed() };
        parameters.dw_y0 = window_rows.start as f64;
        parameters.dw_x1 = layout.cols as f64;
        parameters.dw_y1 = window_rows.end as f64;
        let mut header: grk_header_info = unsafe { std::mem::zeroed() };
        let codec = open_codec(path, &mut parameters, &mut header)?;
        if !unsafe { grk_decompress(codec.0, ptr::null_mut()) } {
            return Err(
                "this JPEG 2000 file cannot be decoded, the server log has Grok's reason".into(),
            );
        }
        let image = codec_image(&codec)?;
        let components = image_components(image)?;
        let rows = window_rows.len();
        let has_the_window = components.len() == layout.samples
            && components.iter().all(|c| {
                c.w as usize == layout.cols
                    && c.h as usize == rows
                    && !c.data.is_null()
                    && (c.data_type == _grk_data_type_GRK_INT_16
                        || c.data_type == _grk_data_type_GRK_INT_32)
            });
        if !has_the_window {
            return Err(format!(
                "Grok decoded rows {window_rows:?} of this JPEG 2000 file to some other shape"
            ));
        }
        let pixel_bytes = layout.samples * format.bytes() as usize;
        let mut pixels = Vec::with_capacity(rows * layout.cols * pixel_bytes);
        for row in 0..rows {
            for col in 0..layout.cols {
                for component in components {
                    let sample = unsafe { sample_at(component, row, col) };
                    format.encode(sample, &mut pixels);
                }
            }
        }
        Ok(pixels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UTM_GRID: &str = r#"<gml:FeatureCollection xmlns:gml="http://www.opengis.net/gml">
      <gml:boundedBy><gml:Envelope srsName="urn:ogc:def:crs:EPSG::32632"/></gml:boundedBy>
      <gml:RectifiedGrid dimension="2">
        <gml:limits><gml:GridEnvelope><gml:low>0 0</gml:low><gml:high>99 99</gml:high></gml:GridEnvelope></gml:limits>
        <gml:axisName>x</gml:axisName><gml:axisName>y</gml:axisName>
        <gml:origin>
          <gml:Point gml:id="origin" srsName="urn:ogc:def:crs:EPSG::32632">
            <gml:pos>500005 5000000</gml:pos>
          </gml:Point>
        </gml:origin>
        <gml:offsetVector srsName="urn:ogc:def:crs:EPSG::32632">10 0</gml:offsetVector>
        <gml:offsetVector srsName="urn:ogc:def:crs:EPSG::32632">0 -20</gml:offsetVector>
      </gml:RectifiedGrid>
    </gml:FeatureCollection>"#;

    #[test]
    fn a_gml_origin_is_the_center_of_the_top_left_pixel() {
        assert_eq!(
            gml_placement(UTM_GRID.as_bytes()),
            Some(GmlPlacement {
                grid: PixelGrid {
                    origin_x: 500000.0,
                    origin_y: 5000010.0,
                    pixel_width: 10.0,
                    pixel_height: 20.0,
                },
                epsg: Some(32632),
            })
        );
    }

    #[test]
    fn gml_vectors_in_latitude_longitude_order_are_read_by_their_shape() {
        let lat_first = r#"<gml:RectifiedGrid xmlns:gml="http://www.opengis.net/gml">
          <gml:origin><gml:Point srsName="http://www.opengis.net/def/crs/EPSG/0/4326">
            <gml:pos>46.995 7.005</gml:pos>
          </gml:Point></gml:origin>
          <gml:offsetVector>0 0.01</gml:offsetVector>
          <gml:offsetVector>-0.01 0</gml:offsetVector>
        </gml:RectifiedGrid>"#;
        let placement = gml_placement(lat_first.as_bytes()).unwrap();
        assert_eq!(placement.epsg, Some(4326));
        assert!((placement.grid.origin_x - 7.0).abs() < 1e-12);
        assert!((placement.grid.origin_y - 47.0).abs() < 1e-12);
        assert_eq!(placement.grid.pixel_width, 0.01);
        assert_eq!(placement.grid.pixel_height, 0.01);
    }

    #[test]
    fn a_rotated_gml_grid_has_no_placement() {
        let rotated = UTM_GRID.replace(">10 0<", ">10 1<");
        assert_eq!(gml_placement(rotated.as_bytes()), None);
    }

    #[test]
    fn xml_without_a_rectified_grid_has_no_placement() {
        assert_eq!(
            gml_placement(b"<metadata><title>scan</title></metadata>"),
            None
        );
        assert_eq!(gml_placement(b"not xml"), None);
    }

    #[test]
    fn srs_names_that_carry_no_epsg_code_name_no_crs() {
        let custom = UTM_GRID.replace("urn:ogc:def:crs:EPSG::32632", "gmljp2://xml/crs.gml#local");
        assert_eq!(gml_placement(custom.as_bytes()).unwrap().epsg, None);
    }

    #[test]
    fn windows_hold_whole_tile_rows_when_a_tile_row_fits_the_budget() {
        // 1024 cols of one band are 4096 bytes a row
        assert_eq!(rows_per_window(1024, 1, 1024).unwrap(), 16384);
        assert_eq!(rows_per_window(1024, 1, 5000).unwrap(), 15000);
    }

    #[test]
    fn a_tile_taller_than_the_budget_is_cut_at_the_budget() {
        assert_eq!(rows_per_window(1024, 1, 40000).unwrap(), 16384);
    }

    #[test]
    fn a_row_past_the_budget_is_refused() {
        assert!(rows_per_window(65535, 300, 1).is_err());
    }

    #[test]
    fn sample_formats_hold_the_precision() {
        assert_eq!(sample_format(8, false), Ok(SampleFormat::U8));
        assert_eq!(sample_format(1, false), Ok(SampleFormat::U16));
        assert_eq!(sample_format(12, false), Ok(SampleFormat::U16));
        assert_eq!(sample_format(16, true), Ok(SampleFormat::I16));
        assert!(sample_format(17, false).is_err());
    }
}
