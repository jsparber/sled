#[macro_use]
extern crate lazy_static;
extern crate quickcheck;
extern crate fail;
extern crate rand;
extern crate sled;
extern crate pagecache;
extern crate tests;

use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;

use quickcheck::{Arbitrary, Gen, QuickCheck, StdGen};

use sled::*;

#[derive(Debug, Clone)]
enum Op {
    Set,
    Del(u8),
    Restart,
    FailPoint(&'static str),
}

use Op::*;

impl Arbitrary for Op {
    fn arbitrary<G: Gen>(g: &mut G) -> Op {
        let fail_points = vec![
            "initial allocation",
            "initial allocation post",
            "zero segment",
            "zero segment post",
            "zero garbage segment",
            "zero garbage segment post",
            "buffer write",
            "buffer write post",
            "write_config bytes",
            "write_config crc",
            "write_config post",
            "trailer write",
            "trailer write post",
            "snap write",
            "snap write len",
            "snap write crc",
            "snap write post",
            "snap write mv",
            "snap write mv post",
            "snap write rm old",
        ];

        if g.gen_weighted_bool(30) {
            return FailPoint(*g.choose(&fail_points).unwrap());
        }

        if g.gen_weighted_bool(10) {
            return Restart;
        }

        let choice = g.gen_range(0, 2);

        match choice {
            0 => Set,
            1 => Del(g.gen::<u8>()),
            _ => panic!("impossible choice"),
        }
    }

    fn shrink(&self) -> Box<Iterator<Item = Op>> {
        match *self {
            Op::Del(ref lid) if *lid > 0 => Box::new(
                vec![
                    Op::Del(*lid / 2),
                    Op::Del(*lid - 1),
                ].into_iter(),
            ),
            _ => Box::new(vec![].into_iter()),
        }
    }
}

fn v(b: &Vec<u8>) -> u16 {
    assert_eq!(b.len(), 2);
    ((b[0] as u16) << 8) + b[1] as u16
}

fn prop_tree_crashes_nicely(ops: Vec<Op>, flusher: bool) -> bool {
    lazy_static! {
        // forces quickcheck to run one thread at a time
        static ref M: Mutex<()> = Mutex::new(());
    }

    let _lock = M.lock().expect("our test lock should not be poisoned");

    // clear all failpoints that may be left over from the last run
    fail::teardown();

    let res = std::panic::catch_unwind(
        || run_tree_crashes_nicely(ops.clone(), flusher),
    );

    fail::teardown();

    match res {
        Err(e) => {
            println!(
                "failed with {:?} on ops {:?} flusher {}",
                e,
                ops,
                flusher
            );
            false
        }
        Ok(res) => {
            if !res {
                println!("failed with ops {:?} flusher: {}", ops, flusher);
            }
            res
        }
    }
}

fn run_tree_crashes_nicely(ops: Vec<Op>, flusher: bool) -> bool {
    let config = ConfigBuilder::new()
        .temporary(true)
        .snapshot_after_ops(1)
        .flush_every_ms(if flusher { Some(1) } else {None})
        .io_buf_size(300)
        .min_items_per_segment(1)
        .blink_fanout(2) // smol pages for smol buffers
        .cache_capacity(40)
        .cache_bits(2)
        .build();

    let mut tree =
        sled::Tree::start(config.clone()).expect("tree should start");
    let mut reference = BTreeMap::new();
    let mut fail_points = HashSet::new();

    macro_rules! restart {
        () => {
            drop(tree);
            let tree_res = sled::Tree::start(config.clone());
            if let Err(ref e) = tree_res {
                if e == &Error::FailPoint {
                    return true;
                }

                println!("could not start database: {}", e);
                return false;
            }

            tree = tree_res.expect("tree should restart");

            let tree_iter = tree.iter().map(|res| {
                let (ref tk, _) = res.expect("should be able to iterate over items in tree");
                v(tk)
            });
            let mut ref_iter = reference.iter().map(|(ref rk, ref rv)| (**rk, **rv));
            for t in tree_iter {
                // make sure the tree value is in there
                while let Some((r, (_rv, certainty))) = ref_iter.next() {
                    if certainty {
                        // tree MUST match reference if we have a certain reference
                        if t != r {
                            println!("expected to iterate over {:?} but got {:?} instead", r, t);
                            return false;
                        }
                        break;
                    } else {
                        // we have an uncertain reference, so we iterate through
                        // it and guarantee the reference is never higher than
                        // the tree value.
                        if t == r {
                            // we can move on to the next tree item
                            break;
                        }

                        if r > t {
                            // we have a bug, the reference iterator should always be <= tree
                            println!("tree verification failed: expected {:?} got {:?}", r, t);
                            return false;
                        }

                        // we are iterating through the reference until we have a certain
                        // item or an item that matches the tree's real item anyway
                    }
                }
            }
        }
    }

    macro_rules! fp_crash {
        ($e:expr) => {
            match $e {
                Ok(thing) => thing,
                Err(Error::FailPoint) => {
                    fail::teardown();
                    restart!();
                    continue;
                }
                other => {
                    println!("got non-failpoint err: {:?}", other);
                    return false;
                },
            }
        }
    }

    // we always increase set_counter because
    let mut set_counter = 0u16;

    for op in ops.into_iter() {
        match op {
            Set => {
                let hi = (set_counter >> 8) as u8;
                let lo = set_counter as u8;

                // insert false certainty until it fully completes
                reference.insert(set_counter, (set_counter, false));

                fp_crash!(tree.set(vec![hi, lo], vec![hi, lo]));

                // make sure we keep the disk and reference in-sync
                // maybe in the future put pending things in their own
                // reference and have a Flush op that syncs them.
                // just because the set above didn't hit a failpoint,
                // it doesn't mean this flush won't hit one, so we
                // also use the fp_crash macro here for handling it.
                fp_crash!(tree.flush());

                // now we should be certain the thing is in there, set certainty to true
                reference.insert(set_counter, (set_counter, true));

                set_counter += 1;
            }
            Del(k) => {
                // insert false certainty before completes
                reference.insert(k as u16, (k as u16, false));

                let res = fp_crash!(tree.del(&*vec![0, k]));
                match res {
                    Some(_) => {
                        // we definitely caused a file write
                        tree.flush().expect("should be able to flush after del")
                    }
                    None => {
                        // we might not have actually written anything
                        // because the key wasn't there.
                        let _ = tree.flush();
                    }
                }

                reference.remove(&(k as u16));
            }
            Restart => {
                restart!();
            }
            FailPoint(fp) => {
                fail_points.insert(fp.clone());
                fail::cfg(&*fp, "return").expect(
                    "should be able to configure failpoint",
                );
            }
        }
    }

    true
}

#[test]
#[cfg(not(target_os = "fuchsia"))]
fn quickcheck_tree_with_failpoints() {
    // use fewer tests for travis OSX builds that stall out all the time
    #[cfg(target_os = "macos")]
    let n_tests = 50;

    #[cfg(not(target_os = "macos"))]
    let n_tests = 100;

    let generator_sz = 100;

    QuickCheck::new()
        .gen(StdGen::new(rand::thread_rng(), generator_sz))
        .tests(n_tests)
        .max_tests(10000)
        .quickcheck(prop_tree_crashes_nicely as fn(Vec<Op>, bool) -> bool);
}

#[test]
fn failpoints_bug_01() {
    // postmortem 1: model did not account for proper reasons to fail to start
    assert!(prop_tree_crashes_nicely(
        vec![FailPoint("snap write"), Restart],
        false,
    ));
}

#[test]
fn failpoints_bug_2() {
    // postmortem 1: the system was assuming the happy path across failpoints
    assert!(prop_tree_crashes_nicely(
        vec![FailPoint("buffer write post"), Set, Set, Restart],
        false,
    ))
}

#[test]
fn failpoints_bug_3() {
    // postmortem 1: this was a regression that happened because we
    // chose to eat errors about advancing snapshots, which trigger
    // log flushes. We should not trigger flushes from snapshots,
    // but first we need to make sure we are better about detecting
    // tears, by not also using 0 as a failed flush signifier.
    assert!(prop_tree_crashes_nicely(
        vec![
            Set,
            Set,
            Set,
            Set,
            FailPoint("trailer write"),
            Set,
            Set,
            Set,
            Set,
            Restart,
        ],
        false,
    ))
}

#[test]
fn failpoints_bug_4() {
    // postmortem 1: the test model was not properly accounting for
    // writes that may-or-may-not be present due to an error.
    assert!(prop_tree_crashes_nicely(
        vec![Set, FailPoint("snap write"), Del(0), Set, Restart],
        false,
    ))
}

#[test]
fn failpoints_bug_5() {
    // postmortem 1:
    assert!(prop_tree_crashes_nicely(
        vec![
            Set,
            FailPoint("snap write mv post"),
            Set,
            FailPoint("snap write"),
            Set,
            Set,
            Set,
            Restart,
            FailPoint("zero segment"),
            Set,
            Set,
            Set,
            Restart,
        ],
        false,
    ))
}

#[test]
fn failpoints_bug_6() {
    // postmortem 1:
    assert!(prop_tree_crashes_nicely(
        vec![
            Set,
            Del(0),
            Set,
            Set,
            Set,
            Restart,
            FailPoint("zero segment post"),
            Set,
            Set,
            Set,
            Restart,
        ],
        false,
    ))
}

#[test]
fn failpoints_bug_7() {
    // postmortem 1: We were crashing because a Segment was
    // in the SegmentAccountant's to_clean Vec, but it had
    // no present pages. This can legitimately happen when
    // a Segment only contains failed log flushes.
    assert!(prop_tree_crashes_nicely(
        vec![
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Del(17),
            Del(29),
            Del(246),
            Del(248),
            Set,
        ],
        false,
    ))
}

#[test]
fn failpoints_bug_8() {
    // postmortem 1: we were assuming that deletes would fail if buffer writes
    // are disabled, but that's not true, because deletes might not cause any
    // writes if the value was not present.
    assert!(prop_tree_crashes_nicely(
        vec![
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Del(0),
            FailPoint("buffer write post"),
            Del(179),
        ],
        false,
    ))
}

#[test]
fn failpoints_bug_9() {
    // postmortem 1: recovery was not properly accounting for
    // ordering issues around allocation and freeing of pages.
    assert!(prop_tree_crashes_nicely(
        vec![
            Set,
            Restart,
            Del(110),
            Del(0),
            Set,
            Restart,
            Set,
            Del(255),
            Set,
            Set,
            Set,
            Set,
            Set,
            Del(38),
            Set,
            Set,
            Del(253),
            Set,
            Restart,
            Set,
            Del(19),
            Set,
            Del(118),
            Set,
            Set,
            Set,
            Set,
            Set,
            Del(151),
            Set,
            Set,
            Del(201),
            Set,
            Restart,
            Set,
            Set,
            Del(17),
            Set,
            Set,
            Set,
            Del(230),
            Set,
            Restart,
        ],
        true,
    ))
}

#[test]
#[ignore]
fn failpoints_bug_10() {
    // expected to iterate over 50 but got 49 instead
    // postmortem 1:
    assert!(prop_tree_crashes_nicely(
        vec![
            Del(175),
            Del(19),
            Restart,
            Del(155),
            Del(111),
            Set,
            Del(4),
            Set,
            Set,
            Set,
            Set,
            Restart,
            Del(94),
            Set,
            Del(83),
            Del(181),
            Del(218),
            Set,
            Set,
            Del(60),
            Del(248),
            Set,
            Set,
            Set,
            Del(167),
            Del(180),
            Del(180),
            Set,
            Restart,
            Del(14),
            Set,
            Set,
            Del(156),
            Del(29),
            Del(190),
            Set,
            Set,
            Del(245),
            Set,
            Del(231),
            Del(95),
            Set,
            Restart,
            Set,
            Del(189),
            Set,
            Restart,
            Set,
            Del(249),
            Set,
            Set,
            Del(110),
            Del(75),
            Set,
            Restart,
            Del(156),
            Del(140),
            Del(101),
            Del(45),
            Del(115),
            Del(162),
            Set,
            Set,
            Del(192),
            Del(31),
            Del(224),
            Set,
            Del(84),
            Del(6),
            Set,
            Del(191),
            Set,
            Set,
            Set,
            Del(86),
            Del(143),
            Del(168),
            Del(175),
            Set,
            Restart,
            Set,
            Set,
            Set,
            Set,
            Set,
            Restart,
            Del(14),
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Del(60),
            Set,
            Del(115),
            Restart,
            Set,
            Del(203),
            Del(12),
            Del(134),
            Del(118),
            FailPoint("trailer write"),
            Del(26),
            Del(161),
            Set,
            Del(6),
            Del(23),
            Set,
            Del(122),
            Del(251),
            Set,
            Restart,
            Set,
            Set,
            Del(252),
            Del(88),
            Set,
            Del(140),
            Del(164),
            Del(203),
            Del(165),
            Set,
            Set,
            Restart,
            Del(0),
            Set,
            Del(146),
            Restart,
            Del(83),
            Restart,
            Del(0),
            Set,
            Del(55),
            Set,
            Set,
            Del(89),
            Set,
            Set,
            Del(105),
            Restart,
            Set,
            Restart,
            Del(145),
            Set,
            Del(17),
            Del(123),
            Set,
            Del(203),
            Set,
            Set,
            Set,
            Set,
            Del(192),
            Del(58),
            Restart,
            Set,
            Restart,
            Set,
            Restart,
            Set,
            Del(142),
            Set,
            Del(220),
            Del(185),
            Set,
            Del(86),
            Set,
            Set,
            Del(123),
            Set,
            Restart,
            Del(56),
            Del(191),
            Set,
            Set,
            Set,
            Set,
            Set,
            Del(123),
            Set,
            Set,
            Set,
            Restart,
            Del(20),
            Del(47),
            Del(207),
            Del(45),
            Set,
            Set,
            Set,
            Del(83),
            Set,
            Del(92),
            Del(117),
            Set,
            Set,
            Restart,
            Del(241),
            FailPoint("trailer write"),
            Set,
            Del(49),
            Set,
            Set,
            Restart,
            Set,
            Set,
            Set,
            Set,
            Del(197),
            Restart,
            Restart,
            Del(192),
            Set,
            Del(10),
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
        ],
        true,
    ))
}

#[test]
#[ignore]
fn failpoints_bug_11() {
    // dupe lsn detected
    // postmortem 1:
    tests::setup_logger();
    assert!(prop_tree_crashes_nicely(
        vec![
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Restart,
            Del(21),
            Set,
            Set,
            FailPoint("buffer write post"),
            Set,
            Set,
            Restart,
        ],
        false,
    ))
}

#[test]
fn failpoints_bug_12() {
    // postmortem 1: we were not sorting the recovery state, which
    // led to divergent state across recoveries. TODO wut
    tests::setup_logger();
    assert!(prop_tree_crashes_nicely(
        vec![
            Set,
            Del(0),
            Set,
            Set,
            Set,
            Set,
            Set,
            Set,
            Restart,
            Set,
            Set,
            Set,
            Restart,
        ],
        false,
    ))
}
