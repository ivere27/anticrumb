//! Pixel observations and the transform from an image to the main-frame viewport.
use super::page::eval;
use crate::{cdp::CdpClient, rpc::pb};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::json;

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct ViewState {
    document: String,
    revision: u64,
    url: String,
    width: f64,
    height: f64,
    scroll_x: f64,
    scroll_y: f64,
    dpr: f64,
    scale: f64,
    offset_x: f64,
    offset_y: f64,
}

fn state(cdp: &mut CdpClient) -> Result<ViewState, String> {
    let token = json!(uuid::Uuid::new_v4().to_string());
    let value = eval(
        cdp,
        &format!(
            r#"
        if (!window.__anticrumbVisual) {{
            const state = {{document:{token},revision:0}};
            new MutationObserver(() => state.revision++).observe(document, {{subtree:true,childList:true,attributes:true,characterData:true}});
            for (const event of ['scroll','input','change']) document.addEventListener(event, () => state.revision++, true);
            window.__anticrumbVisual = state;
        }}
        const s=window.__anticrumbVisual, v=window.visualViewport;
        return {{document:s.document,revision:s.revision,url:location.href,
          width:innerWidth,height:innerHeight,scroll_x:scrollX,scroll_y:scrollY,
          dpr:devicePixelRatio,scale:v?.scale ?? 1,offset_x:v?.offsetLeft ?? 0,offset_y:v?.offsetTop ?? 0}};
    "#
        ),
    )?;
    serde_json::from_value(value).map_err(|e| format!("invalid viewport metrics: {e}"))
}

struct Frame {
    id: String,
    width: u32,
    height: u32,
    region: pb::BrowserRegion,
    view: ViewState,
}

#[derive(Default)]
pub(super) struct VisualMode {
    pub enabled: bool,
    frame: Option<Frame>,
}

impl VisualMode {
    pub fn invalidate(&mut self) {
        self.frame = None;
    }

    pub fn point(
        &self,
        cdp: &mut CdpClient,
        point: &pb::BrowserPoint,
    ) -> Result<(f64, f64), String> {
        let frame = self
            .frame
            .as_ref()
            .filter(|f| f.id == point.screenshot_id)
            .ok_or("stale or unknown screenshot; take a new screenshot before acting")?;
        if state(cdp)? != frame.view {
            return Err(
                "page changed since screenshot; take a new screenshot before acting".into(),
            );
        }
        map_point(&frame.region, frame.width, frame.height, point.x, point.y)
    }

    pub fn capture(
        &mut self,
        cdp: &mut CdpClient,
        region: Option<&pb::BrowserRegion>,
    ) -> Result<pb::BrowserScreenshot, String> {
        self.invalidate();
        for _ in 0..3 {
            let view = state(cdp)?;
            if !view.dpr.is_finite()
                || view.dpr <= 0.0
                || view.scale != 1.0
                || view.offset_x != 0.0
                || view.offset_y != 0.0
            {
                return Err(
                    "unsupported visual viewport; reset pinch zoom before capturing".into(),
                );
            }
            let region = region.copied().unwrap_or(pb::BrowserRegion {
                x: 0.0,
                y: 0.0,
                width: view.width,
                height: view.height,
            });
            if ![region.x, region.y, region.width, region.height]
                .iter()
                .all(|v| v.is_finite())
                || region.x < 0.0
                || region.y < 0.0
                || region.width < 1.0
                || region.height < 1.0
                || region.width > 4096.0
                || region.height > 4096.0
                || region.x + region.width > view.width
                || region.y + region.height > view.height
            {
                return Err("capture region must be inside the current viewport and at most 4096x4096 CSS pixels".into());
            }
            let result = cdp.call(
                "Page.captureScreenshot",
                json!({
                    "format":"png", "fromSurface":true, "captureBeyondViewport":false,
                    "clip":{"x":view.scroll_x+region.x,"y":view.scroll_y+region.y,
                        "width":region.width,"height":region.height,"scale":1.0/view.dpr}
                }),
            )?;
            let encoded = result["data"]
                .as_str()
                .ok_or("screenshot returned no PNG data")?;
            if encoded.len() > (crate::input::MAX_FILE_BYTES as usize).div_ceil(3) * 4 {
                return Err("screenshot exceeds 32 MiB; capture a smaller region".into());
            }
            let png = STANDARD
                .decode(encoded)
                .map_err(|e| format!("invalid screenshot: {e}"))?;
            let (width, height) = png_size(&png)?;
            let mut after = state(cdp)?;
            // DOM updates do not prevent observing a live page. Keep the
            // pre-capture revision in the frame so clicks still reject changes.
            after.revision = view.revision;
            if after != view {
                continue;
            }
            let id = format!("shot_{}", uuid::Uuid::new_v4().simple());
            self.frame = Some(Frame {
                id: id.clone(),
                width,
                height,
                region,
                view,
            });
            self.enabled = true;
            return Ok(pb::BrowserScreenshot {
                id,
                artifact_id: String::new(),
                width,
                height,
                region: Some(region),
                png,
            });
        }
        Err(
            "page or viewport kept moving during capture; wait for it to settle and take another screenshot"
                .into(),
        )
    }
}

fn png_size(png: &[u8]) -> Result<(u32, u32), String> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" || &png[12..16] != b"IHDR" {
        return Err("browser returned an invalid PNG".into());
    }
    let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return Err("screenshot dimensions exceed 4096x4096 pixels".into());
    }
    Ok((width, height))
}

fn map_point(
    region: &pb::BrowserRegion,
    width: u32,
    height: u32,
    x: f64,
    y: f64,
) -> Result<(f64, f64), String> {
    if !x.is_finite()
        || !y.is_finite()
        || x < 0.0
        || y < 0.0
        || x >= f64::from(width)
        || y >= f64::from(height)
    {
        return Err("point is outside the screenshot; use image pixel coordinates".into());
    }
    Ok((
        region.x + x * region.width / f64::from(width),
        region.y + y * region.height / f64::from(height),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cropped_scaled_images_map_to_viewport_and_reject_outside_pixels() {
        let region = pb::BrowserRegion {
            x: 200.0,
            y: 100.0,
            width: 600.0,
            height: 400.0,
        };
        assert_eq!(
            map_point(&region, 1200, 800, 100.0, 60.0).unwrap(),
            (250.0, 130.0)
        );
        assert_eq!(
            map_point(&region, 300, 200, 100.0, 60.0).unwrap(),
            (400.0, 220.0)
        );
        for (x, y) in [(-1.0, 0.0), (1200.0, 0.0), (0.0, 800.0), (f64::NAN, 0.0)] {
            assert!(map_point(&region, 1200, 800, x, y).is_err());
        }
    }
}
