use core::{convert::Infallible, ops::Deref};

use cortex_m::delay::Delay;
use embedded_hal::digital::v2::InputPin;

use crate::{debounce::Debounce, key_mapping, keyboard::KbHidReport};

#[derive(Clone, Copy)]
pub struct KeyScan<const NUM_ROWS: usize, const NUM_COLS: usize> {
    matrix: [[bool; NUM_ROWS]; NUM_COLS],
}

impl<const NUM_ROWS: usize, const NUM_COLS: usize> Deref for KeyScan<NUM_ROWS, NUM_COLS> {
    type Target = [[bool; NUM_ROWS]; NUM_COLS];

    fn deref(&self) -> &Self::Target {
        &self.matrix
    }
}

impl<const NUM_ROWS: usize, const NUM_COLS: usize> KeyScan<NUM_ROWS, NUM_COLS> {
    pub fn scan(
        rows: &[&dyn InputPin<Error = Infallible>],
        columns: &mut [&mut dyn embedded_hal::digital::v2::OutputPin<Error = Infallible>],
        delay: &mut Delay,
        debounce: &mut Debounce<NUM_ROWS, NUM_COLS>,
    ) -> Self {
        let mut raw_matrix = [[false; NUM_ROWS]; NUM_COLS];

        for (gpio_col, matrix_col) in columns.iter_mut().zip(raw_matrix.iter_mut()) {
            gpio_col.set_high().unwrap();
            delay.delay_us(10);

            for (gpio_row, matrix_row) in rows.iter().zip(matrix_col.iter_mut()) {
                *matrix_row = gpio_row.is_high().unwrap();
            }

            gpio_col.set_low().unwrap();
            delay.delay_us(10);
        }

        let matrix = debounce.report_and_tick(&raw_matrix);
        Self { matrix }
    }
}

impl<const NUM_ROWS: usize, const NUM_COLS: usize> From<KeyScan<NUM_ROWS, NUM_COLS>>
    for KbHidReport
{
    fn from(scan: KeyScan<NUM_ROWS, NUM_COLS>) -> Self {
        let layer_mapping = if scan.matrix[0][5] {
            key_mapping::FN_LAYER_MAPPING
        } else {
            key_mapping::NORMAL_LAYER_MAPPING
        };

        let mut report = KbHidReport::default();

        for (matrix_column, mapping_column) in scan.matrix.iter().zip(layer_mapping) {
            for (key_pressed, mapping_row) in matrix_column.iter().zip(mapping_column) {
                if *key_pressed {
                    report.pressed(mapping_row);
                }
            }
        }

        report
    }
}
