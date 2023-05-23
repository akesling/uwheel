use clap::Parser;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[clap(short, long, value_parser, default_value_t = false)]
    drill_down: bool,
}

use std::time::Instant;

use haw::{time::NumericalDuration, *};

//pub type SerdeAggregator = haw::aggregator::U64SumAggregator;
pub type SerdeAggregator = haw::aggregator::AllAggregator;

fn main() {
    let args = Args::parse();
    // Initial start time
    let time = 0;

    let mut wheel = if args.drill_down {
        Wheel::<SerdeAggregator>::with_drill_down(time)
    } else {
        Wheel::<SerdeAggregator>::new(time)
    };

    println!(
        "Memory size Wheel {} bytes",
        std::mem::size_of::<Wheel<SerdeAggregator>>()
    );

    println!(
        "Memory Seconds Wheel {} bytes",
        std::mem::size_of::<SecondsWheel<SerdeAggregator>>()
    );
    println!(
        "Memory Minutes Wheel {} bytes",
        std::mem::size_of::<MinutesWheel<SerdeAggregator>>()
    );
    println!(
        "Memory Hours Wheel {} bytes",
        std::mem::size_of::<HoursWheel<SerdeAggregator>>()
    );
    println!(
        "Memory Days Wheel {} bytes",
        std::mem::size_of::<DaysWheel<SerdeAggregator>>()
    );
    println!(
        "Memory Weeks Wheel {} bytes",
        std::mem::size_of::<WeeksWheel<SerdeAggregator>>()
    );
    println!(
        "Memory Years Wheel {} bytes",
        std::mem::size_of::<YearsWheel<SerdeAggregator>>()
    );

    let now = Instant::now();
    let raw_wheel = wheel.as_bytes();
    println!(
        "Serialised empty wheel size {} bytes in {:?}",
        raw_wheel.len(),
        now.elapsed()
    );

    let now = Instant::now();
    let raw_wheel_native = wheel.to_be_bytes();
    println!(
        "Native: Serialised empty wheel size {} bytes in {:?}",
        raw_wheel_native.len(),
        now.elapsed()
    );
    let now = Instant::now();
    let lz4_compressed = lz4_flex::compress_prepend_size(&raw_wheel_native);
    println!(
        "Native: Empty lz4 serialised wheel size {} bytes in {:?}",
        lz4_compressed.len(),
        now.elapsed(),
    );

    let now = Instant::now();
    let lz4_compressed = lz4_flex::compress_prepend_size(&raw_wheel);
    println!(
        "Empty lz4 serialised wheel size {} bytes in {:?}",
        lz4_compressed.len(),
        now.elapsed(),
    );

    let now = Instant::now();
    let lz4_decompressed = lz4_flex::decompress_size_prepended(&lz4_compressed).unwrap();
    let lz4_wheel = Wheel::<SerdeAggregator>::from_bytes(&lz4_decompressed).unwrap();
    println!(
        "Empty lz4 decompress and deserialise wheel in {:?}",
        now.elapsed(),
    );
    assert!(lz4_wheel.is_empty());

    let total_ticks = wheel.remaining_ticks();

    for _ in 0..total_ticks - 1 {
        wheel.advance(1.seconds());
        wheel
            .insert(Entry::new(1.0, wheel.watermark() + 1))
            .unwrap();
    }
    println!("wheel total {:?}", wheel.landmark());

    let raw_seconds_wheel = wheel.seconds_unchecked().as_bytes();
    println!(
        "Serialised Seconds wheel size {} bytes",
        raw_seconds_wheel.len()
    );

    let now = Instant::now();
    let raw_wheel_native = wheel.to_be_bytes();
    println!(
        "Native: Full serialised wheel size {} bytes in {:?}",
        raw_wheel_native.len(),
        now.elapsed()
    );
    let now = Instant::now();
    let lz4_compressed = lz4_flex::compress_prepend_size(&raw_wheel_native);
    println!(
        "Native: Full lz4 serialised wheel size bytes {} in {:?}",
        lz4_compressed.len(),
        now.elapsed(),
    );

    let now = Instant::now();
    let raw_wheel = wheel.as_bytes();
    println!(
        "Full serialised wheel size {} bytes in {:?}",
        raw_wheel.len(),
        now.elapsed()
    );

    let now = Instant::now();
    let lz4_compressed = lz4_flex::compress_prepend_size(&raw_wheel);
    println!(
        "Full lz4 serialised wheel size bytes {} in {:?}",
        lz4_compressed.len(),
        now.elapsed(),
    );

    let now = Instant::now();
    let lz4_decompressed = lz4_flex::decompress_size_prepended(&lz4_compressed).unwrap();
    let lz4_wheel = Wheel::<SerdeAggregator>::from_bytes(&lz4_decompressed).unwrap();
    println!(
        "Full lz4 decompress and deserialise wheel in {:?}",
        now.elapsed(),
    );
    println!("deserialised wheel total {:?}", lz4_wheel.landmark());
}
