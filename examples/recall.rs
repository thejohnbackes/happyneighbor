use byteorder::{ByteOrder, LittleEndian};
use generic_array::{typenum, ArrayLength};
use gnuplot::*;
use hnsw::*;
use rand::distributions::Standard;
use rand::{Rng, SeedableRng};
use rand_pcg::Pcg64;
use std::cell::RefCell;
use std::io::Read;
use std::path::PathBuf;
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(name = "recall", about = "Generates recall graphs for HNSW")]
struct Opt {
    /// The value of M to use.
    ///
    /// This can only be between 4 and 52 inclusive and a multiple of 4.
    /// M0 is set to 2 * M.
    #[structopt(short = "m", long = "max_edges", default_value = "24")]
    m: usize,
    /// The dataset size to test on.
    #[structopt(short = "s", long = "size", default_value = "10000")]
    size: usize,
    /// Total number of query bitstrings.
    ///
    /// The higher this is, the better the quality of the output data and statistics, but
    /// the longer the benchmark will take to set up.
    #[structopt(short = "q", long = "queries", default_value = "10000")]
    num_queries: usize,
    /// The number of dimensions in the feature vector.
    ///
    /// This is the length of the feature vector. The descriptor_stride (-d)
    /// parameter must exceed this value.
    ///
    /// Possible values:
    /// - 8
    /// - 16
    /// - 32
    /// - 64
    /// - 128
    /// - 256
    /// - 512
    #[structopt(short = "l", long = "dimensions", default_value = "64")]
    dimensions: usize,
    /// The beginning ef value.
    #[structopt(short = "b", long = "beginning_ef", default_value = "1")]
    beginning_ef: usize,
    /// The ending ef value.
    #[structopt(short = "e", long = "ending_ef", default_value = "64")]
    ending_ef: usize,
    /// The number of nearest neighbors.
    #[structopt(short = "k", long = "neighbors", default_value = "2")]
    k: usize,
    /// Use the following file to load the search space.
    #[structopt(short = "f", long = "file")]
    file: Option<PathBuf>,
    /// The descriptor stride length in floats.
    ///
    /// KAZE: 64
    /// SIFT: 128
    #[structopt(short = "d", long = "descriptor_stride", default_value = "64")]
    descriptor_stride: usize,
}

fn process<M: ArrayLength<u32>, M0: ArrayLength<u32>>(opt: &Opt) -> (Vec<f64>, Vec<f64>) {
    assert!(
        opt.k <= opt.size,
        "You must choose a dataset size larger or equal to the test search size"
    );
    let rng = Pcg64::from_seed([5; 32]);

    let (search_space, query_strings): (Vec<f32>, Vec<f32>) = if let Some(filepath) = &opt.file {
        eprintln!(
            "Reading {} search space descriptors of size {} f32s from file \"{}\"...",
            opt.size,
            opt.descriptor_stride,
            filepath.display()
        );
        let mut file = std::fs::File::open(filepath).expect("unable to open file");
        // We are loading floats, so multiply by 4.
        let mut search_space = vec![0u8; opt.size * opt.descriptor_stride * 4];
        file.read_exact(&mut search_space).expect(
            "unable to read enough search descriptors from the file (try decreasing -s/-q)",
        );
        let search_space = search_space
            .chunks_exact(4)
            .map(LittleEndian::read_f32)
            .collect();
        eprintln!("Done.");

        eprintln!(
            "Reading {} query descriptors of size {} f32s from file \"{}\"...",
            opt.num_queries,
            opt.descriptor_stride,
            filepath.display()
        );
        // We are loading floats, so multiply by 4.
        let mut query_strings = vec![0u8; opt.num_queries * opt.descriptor_stride * 4];
        file.read_exact(&mut query_strings)
            .expect("unable to read enough query descriptors from the file (try decreasing -q/-s)");
        let query_strings = query_strings
            .chunks_exact(4)
            .map(LittleEndian::read_f32)
            .collect();
        eprintln!("Done.");

        (search_space, query_strings)
    } else {
        eprintln!("Generating {} random bitstrings...", opt.size);
        let search_space: Vec<f32> = rng
            .sample_iter(&Standard)
            .take(opt.size * opt.descriptor_stride)
            .collect();
        eprintln!("Done.");

        // Create another RNG to prevent potential correlation.
        let rng = Pcg64::from_seed([6; 32]);

        eprintln!(
            "Generating {} independent random query strings...",
            opt.num_queries
        );
        let query_strings: Vec<f32> = rng
            .sample_iter(&Standard)
            .take(opt.num_queries * opt.descriptor_stride)
            .collect();
        eprintln!("Done.");
        (search_space, query_strings)
    };

    let search_space: Vec<_> = search_space
        .chunks_exact(opt.descriptor_stride)
        .map(|c| &c[..opt.dimensions])
        .map(Euclidean)
        .collect();
    let query_strings: Vec<_> = query_strings
        .chunks_exact(opt.descriptor_stride)
        .map(|c| &c[..opt.dimensions])
        .map(Euclidean)
        .collect();

    eprintln!(
        "Computing the correct nearest neighbor distance for all {} queries...",
        opt.num_queries
    );
    let correct_worst_distances: Vec<f32> = query_strings
        .iter()
        .cloned()
        .map(|feature| {
            let mut v = vec![];
            for distance in search_space
                .iter()
                .map(|n| FloatingDistance::floating_distance(n, &feature))
            {
                let pos = v
                    .binary_search_by_key(&float_ord::FloatOrd(distance), |&distance| {
                        float_ord::FloatOrd(distance)
                    })
                    .unwrap_or_else(|e| e);
                v.insert(pos, distance);
                if v.len() > opt.k {
                    v.resize_with(opt.k, || unreachable!());
                }
            }
            // Get the worst distance
            v.into_iter().take(opt.k).last().unwrap()
        })
        .collect();
    eprintln!("Done.");

    eprintln!("Generating HNSW...");
    let mut hnsw: HNSW<_, M, M0> = HNSW::new();
    let mut searcher: Searcher = Searcher::default();
    for feature in &search_space {
        hnsw.insert(feature.clone(), &mut searcher);
    }
    eprintln!("Done.");

    eprintln!("Computing recall graph...");
    let efs = opt.beginning_ef..=opt.ending_ef;
    let state = RefCell::new((searcher, query_strings.iter().cloned().enumerate().cycle()));
    let (recalls, times): (Vec<f64>, Vec<f64>) = efs
        .map(|ef| {
            let correct = RefCell::new(0usize);
            let dest = vec![!0; opt.k];
            let stats = easybench::bench_env(dest, |mut dest| {
                let mut refmut = state.borrow_mut();
                let (searcher, query) = &mut *refmut;
                let (ix, query_feature) = query.next().unwrap();
                let correct_worst_distance = correct_worst_distances[ix];
                // Go through all the features.
                for &mut feature_ix in hnsw.nearest(&query_feature, ef, searcher, &mut dest) {
                    // Any feature that is less than or equal to the worst real nearest neighbor distance is correct.
                    if FloatingDistance::floating_distance(
                        &search_space[feature_ix as usize],
                        &query_feature,
                    ) <= correct_worst_distance
                    {
                        *correct.borrow_mut() += 1;
                    }
                }
            });
            (stats, correct.into_inner())
        })
        .fold(
            (vec![], vec![]),
            |(mut recalls, mut times), (stats, correct)| {
                times.push((stats.ns_per_iter * 0.1f64.powi(9)).recip());
                // The maximum number of correct nearest neighbors is
                recalls.push(correct as f64 / (stats.iterations * opt.k) as f64);
                (recalls, times)
            },
        );
    eprintln!("Done.");

    (recalls, times)
}

fn main() {
    let opt = Opt::from_args();

    let (recalls, times) = {
        use typenum::*;
        // This can be increased indefinitely at the expense of compile time.
        match opt.m {
            4 => process::<U4, U8>(&opt),
            8 => process::<U8, U16>(&opt),
            12 => process::<U12, U24>(&opt),
            16 => process::<U16, U32>(&opt),
            20 => process::<U20, U40>(&opt),
            24 => process::<U24, U48>(&opt),
            28 => process::<U28, U56>(&opt),
            32 => process::<U32, U64>(&opt),
            36 => process::<U36, U72>(&opt),
            40 => process::<U40, U80>(&opt),
            44 => process::<U44, U88>(&opt),
            48 => process::<U48, U96>(&opt),
            52 => process::<U52, U104>(&opt),
            _ => {
                eprintln!("Only M between 4 and 52 inclusive and multiples of 4 are allowed");
                return;
            }
        }
    };

    let mut fg = Figure::new();

    fg.axes2d()
        .set_title(
            &format!(
                "{}-NN Recall Graph (dimensions = {}, size = {}, M = {})",
                opt.k, opt.dimensions, opt.size, opt.m
            ),
            &[],
        )
        .set_x_label("Recall Rate", &[])
        .set_y_label("Lookups per second", &[])
        .lines(&recalls, &times, &[LineWidth(2.0), Color("blue")])
        .set_y_ticks(Some((Auto, 2)), &[], &[])
        .set_grid_options(true, &[LineStyle(DotDotDash), Color("black")])
        .set_minor_grid_options(&[LineStyle(SmallDot), Color("red")])
        .set_x_grid(true)
        .set_y_grid(true)
        .set_y_minor_grid(true);

    fg.show();
}
