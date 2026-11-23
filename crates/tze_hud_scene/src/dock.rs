//! Geometry shared by dock admission, presentation and pointer routing.

use crate::types::Rect;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DockMetrics {
    pub row_height: f32,
    pub caption_width: f32,
    pub gap: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DockLayout {
    pub column: Rect,
    pub capacity: usize,
    pub scale: f32,
    metrics: DockMetrics,
}

impl DockLayout {
    /// Metrics are already converted to physical pixels for this display.
    /// Aspect ratios are from eligible bundles in owner-configured order.
    pub fn new(
        display: Rect,
        max_rows: usize,
        metrics: DockMetrics,
        aspect_ratios: impl IntoIterator<Item = f32>,
    ) -> Self {
        let widest_row = aspect_ratios
            .into_iter()
            .fold(metrics.caption_width, |width, aspect| {
                width + metrics.gap + metrics.row_height * aspect
            });
        let width_limit = display.width * 0.35;
        let scale = if widest_row > width_limit {
            width_limit / widest_row
        } else {
            1.0
        };
        let metrics = DockMetrics {
            row_height: metrics.row_height * scale,
            caption_width: metrics.caption_width * scale,
            gap: metrics.gap * scale,
        };
        let column = Rect::new(
            display.x + display.width * 0.05,
            display.y + display.height * 0.12,
            widest_row * scale,
            display.height * 0.73,
        );
        let pitch = metrics.row_height + metrics.gap;
        let capacity = if pitch > 0.0 {
            max_rows.min(((column.height + metrics.gap) / pitch).floor() as usize)
        } else {
            0
        };
        Self {
            column,
            capacity,
            scale,
            metrics,
        }
    }

    pub fn caption(&self, row: usize) -> Rect {
        Rect::new(
            self.column.x,
            self.column.y + row as f32 * (self.metrics.row_height + self.metrics.gap),
            self.metrics.caption_width,
            self.metrics.row_height,
        )
    }

    /// Place a widget after its live predecessors without moving earlier items.
    pub fn widget(
        &self,
        row: usize,
        preceding: impl IntoIterator<Item = f32>,
        aspect_ratio: f32,
    ) -> Rect {
        let caption = self.caption(row);
        let x = preceding
            .into_iter()
            .fold(caption.x + caption.width + self.metrics.gap, |x, aspect| {
                x + self.metrics.row_height * aspect + self.metrics.gap
            });
        Rect::new(
            x,
            caption.y,
            self.metrics.row_height * aspect_ratio,
            self.metrics.row_height,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(display_scale: f32) -> DockMetrics {
        DockMetrics {
            row_height: 52.0 * display_scale,
            caption_width: 96.0 * display_scale,
            gap: 8.0 * display_scale,
        }
    }

    #[test]
    fn dock_layout_capacity_follows_usable_height_and_configured_limit() {
        for (width, height, scale, max_rows, rows) in [
            (3840.0, 2160.0, 1.5, 32, 17),
            (3840.0, 2160.0, 1.5, 16, 16),
            (1920.0, 1080.0, 1.0, 32, 13),
        ] {
            let layout = DockLayout::new(
                Rect::new(0.0, 0.0, width, height),
                max_rows,
                metrics(scale),
                [360.0 / 52.0, 160.0 / 52.0],
            );
            assert_eq!(layout.capacity, rows);
            let last = layout.caption(rows - 1);
            assert!(last.is_within(&layout.column));
            if rows < max_rows {
                assert!(!layout.caption(rows).is_within(&layout.column));
            }
        }
    }

    #[test]
    fn dock_layout_uses_monitor_origin_and_scales_wide_rows_uniformly() {
        let monitor = Rect::new(-960.0, 100.0, 960.0, 600.0);
        let layout = DockLayout::new(monitor, 32, metrics(1.0), [7.0, 4.0]);
        assert_eq!((layout.column.x, layout.column.y), (-912.0, 172.0));
        assert!((layout.column.width - 336.0).abs() < 0.01);
        assert!(layout.scale < 1.0);
        let progress = layout.widget(0, [], 7.0);
        let countdown = layout.widget(0, [7.0], 4.0);
        let next = layout.widget(1, [], 7.0);
        assert_eq!(progress.height, countdown.height);
        assert_eq!(progress.height, 52.0 * layout.scale);
        assert_eq!(progress.width / progress.height, 7.0);
        assert!(countdown.x > progress.x + progress.width);
        assert!(next.y > progress.y + progress.height);
        assert_eq!(next.x, progress.x);
    }
}
