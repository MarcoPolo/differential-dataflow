//! A trie-structured representation of update tuples with hashable keys. 
//! 
//! One goal of this representation is to allow multiple kinds of types of hashable keys, including
//! keys that implement `Hash`, keys whose hashes have been computed and are stashed with the key, and
//! integers keys which are promised to be random enough to be used as the hashes themselves.
use std::rc::Rc;

use timely_sort::{Unsigned};

use hashable::HashOrdered;

use trace::layers::{Trie, TupleBuilder};
use trace::layers::Builder as TrieBuilder;
use trace::layers::Cursor as TrieCursor;
use trace::layers::hashed::{HashedLayer, HashedBuilder, HashedCursor};
use trace::layers::ordered::{OrderedLayer, OrderedBuilder, OrderedCursor};
use trace::layers::unordered::{UnorderedLayer, UnorderedBuilder, UnorderedCursor};

use lattice::Lattice;
use trace::{Batch, Builder, Cursor, Trace};
use trace::consolidate;
use trace::description::Description;
use trace::cursor::cursor_list::CursorList;

type RHHBuilder<Key, Val, Time> = HashedBuilder<Key, OrderedBuilder<Val, UnorderedBuilder<(Time, isize)>>>;

/// An append-only collection of update tuples.
///
/// A spine maintains a small number of immutable collections of update tuples, merging the collections when
/// two have similar sizes. In this way, it allows the addition of more tuples, which may then be merged with
/// other immutable collections. 
#[derive(Debug)]
pub struct Spine<Key: HashOrdered, Val: Ord, Time: Lattice+Ord> {
	frontier: Vec<Time>,					// Times after which the times in the traces must be distinguishable.
	layers: Vec<Rc<Layer<Key, Val, Time>>>	// Several possibly shared collections of updates.
}

// A trace implementation for any key type that can be borrowed from or converted into `Key`.
impl<Key, Val, Time> Trace<Key, Val, Time> for Spine<Key, Val, Time> 
where 
	Key: Clone+Default+HashOrdered+'static,
	Val: Ord+Clone+'static, 
	Time: Lattice+Ord+Clone+Default+'static,
{

	type Batch = Rc<Layer<Key, Val, Time>>;
	type Cursor = CursorList<Key, Val, Time, LayerCursor<Key, Val, Time>>;

	fn new(default: Time) -> Self {
		Spine { 
			frontier: vec![default],
			layers: Vec::new(),
		} 		
	}
	// Note: this does not perform progressive merging; that code is around somewhere though.
	fn insert(&mut self, layer: Self::Batch) {
		if layer.layer.keys() > 0 {
			// while last two elements exist, both less than layer.len()
			while self.layers.len() >= 2 && self.layers[self.layers.len() - 2].len() < layer.len() {
				let layer1 = self.layers.pop().unwrap();
				let layer2 = self.layers.pop().unwrap();
				let result = Rc::new(Layer::merge(&layer1, &layer2));
				if result.len() > 0 {
					self.layers.push(result);
				}
			}

			if layer.len() > 0 {
				self.layers.push(layer);
			}

		    while self.layers.len() >= 2 && self.layers[self.layers.len() - 2].len() < 2 * self.layers[self.layers.len() - 1].len() {
				let layer1 = self.layers.pop().unwrap();
				let layer2 = self.layers.pop().unwrap();
				let mut result = Rc::new(layer1.merge(&layer2));

				// if we just merged the last layer, `advance_by` it.
				if self.layers.len() == 0 {
					result = Rc::new(Layer::<Key, Val, Time>::advance_by(&result, &self.frontier[..]));
				}

				if result.len() > 0 {
					self.layers.push(result);
				}
			}
		}
	}
	fn cursor(&self) -> Self::Cursor {
		let mut cursors = Vec::new();
		for layer in &self.layers[..] {
			if layer.len() > 0 {
				cursors.push(LayerCursor { cursor: layer.layer.cursor() } );
			}
		}

		CursorList::new(cursors)
	}
	fn advance_by(&mut self, frontier: &[Time]) {
		self.frontier = frontier.to_vec();
	}
}


/// An immutable collection of update tuples, from a contiguous interval of logical times.
#[derive(Debug)]
pub struct Layer<Key: HashOrdered, Val: Ord, Time: Lattice+Ord> {
	/// Where all the dataz is.
	pub layer: HashedLayer<Key, OrderedLayer<Val, UnorderedLayer<(Time, isize)>>>,
	/// Description of the update times this layer represents.
	pub desc: Description<Time>,
}

impl<Key: Clone+Default+HashOrdered, Val: Ord+Clone, Time: Lattice+Ord+Clone+Default> Batch<Key, Val, Time> for Rc<Layer<Key, Val, Time>> {
	type Builder = LayerBuilder<Key, Val, Time>;
	type OrderedBuilder = OrdBuilder<Key, Val, Time>;
	type Cursor = LayerCursor<Key, Val, Time>;
	fn cursor(&self) -> Self::Cursor {  LayerCursor { cursor: self.layer.cursor() } }
	fn len(&self) -> usize { self.layer.tuples() }
}

impl<Key: Clone+Default+HashOrdered, Val: Ord+Clone, Time: Lattice+Ord+Clone+Default> Layer<Key, Val, Time> {

	/// Conducts a full merge, right away. Times not advanced.
	pub fn merge(&self, other: &Self) -> Self {
		Layer {
			layer: self.layer.merge(&other.layer),
			desc: Description::new(&[], &[], &[]),
		}
	}
	/// Advances times in `layer` and consolidates differences for like times.
	///
	/// TODO: This method could be defined on `&mut self`, exploiting in-place mutation
	/// to avoid allocation and building headaches. It is implemented on the `Rc` variant
	/// to get access to `cursor()`, and in principle to allow a progressive implementation. 
	pub fn advance_by(layer: &Rc<Self>, frontier: &[Time]) -> Self { 

		// TODO: This is almost certainly too much `with_capacity`.
		// TODO: We should design and implement an "in-order builder", which takes cues from key and val
		// structure, rather than having to re-infer them from tuples.
		// TODO: We should understand whether in-place mutation is appropriate, or too gross. At the moment,
		// this could be a general method defined on any implementor of `trace::Cursor`.
		let mut builder = <RHHBuilder<Key, Val, Time> as TupleBuilder>::with_capacity(layer.len());

		if layer.len() > 0 {
			let mut times = Vec::new();
			let mut cursor = layer.cursor();

			while cursor.key_valid() {
				while cursor.val_valid() {
					cursor.map_times(|time: &Time, diff| times.push((time.advance_by(frontier).unwrap(), diff)));
					consolidate(&mut times, 0);
					for (time, diff) in times.drain(..) {
						let key_ref: &Key = cursor.key();
						let key_clone: Key = key_ref.clone();
						let val_ref: &Val = cursor.val();
						let val_clone: Val = val_ref.clone();
						builder.push_tuple((key_clone, (val_clone, (time, diff))));
					}
					cursor.step_val()
				}
				cursor.step_key();
			}
		}

		Layer { 
			layer: builder.done(), 
			desc: Description::new(layer.desc.lower(), layer.desc.upper(), frontier),
		}
	}
}

/// A cursor for navigating a single layer.
#[derive(Debug)]
pub struct LayerCursor<Key: Clone+HashOrdered, Val: Ord+Clone, Time: Lattice+Ord+Clone> {
	cursor: HashedCursor<Key, OrderedCursor<Val, UnorderedCursor<(Time, isize)>>>,
}


impl<Key: Clone+HashOrdered, Val: Ord+Clone, Time: Lattice+Ord+Clone> Cursor<Key, Val, Time> for LayerCursor<Key, Val, Time> {
	fn key(&self) -> &Key { &self.cursor.key() }
	fn val(&self) -> &Val { self.cursor.child.key() }
	fn map_times<L: FnMut(&Time, isize)>(&mut self, mut logic: L) {
		self.cursor.child.child.rewind();
		while self.cursor.child.child.valid() {
			logic(&self.cursor.child.child.key().0, self.cursor.child.child.key().1);
			self.cursor.child.child.step();
		}
	}
	fn key_valid(&self) -> bool { self.cursor.valid() }
	fn val_valid(&self) -> bool { self.cursor.child.valid() }
	fn step_key(&mut self){ self.cursor.step(); }
	fn seek_key(&mut self, key: &Key) { self.cursor.seek(key); }
	fn step_val(&mut self) { self.cursor.child.step(); }
	fn seek_val(&mut self, val: &Val) { self.cursor.child.seek(val); }
	fn rewind_keys(&mut self) { self.cursor.rewind(); }
	fn rewind_vals(&mut self) { self.cursor.child.rewind(); }
}


/// A builder for creating layers from unsorted update tuples.
pub struct LayerBuilder<K: HashOrdered, V: Ord, T: Ord> {
	time: T,
	// TODO : Have this build `Layer`s and merge, instead of re-sorting sorted data.
    buffer: Vec<((K, V), isize)>,
    buffers: Vec<Vec<((K, V), isize)>>,
}

impl<Key, Val, Time> Builder<Key, Val, Time, Rc<Layer<Key, Val, Time>>> for LayerBuilder<Key, Val, Time> 
where Key: Clone+Default+HashOrdered, Val: Ord+Clone, Time: Lattice+Ord+Clone+Default {
	fn new() -> Self { LayerBuilder { time: Default::default(), buffer: Vec::new(), buffers: Vec::new() } }
	fn push(&mut self, (key, val, time, diff): (Key, Val, Time, isize)) {
		self.time = time;
		self.buffer.push(((key, val), diff));
		if self.buffer.len() == 1 << 12 {
			self.buffers.push(::std::mem::replace(&mut self.buffer, Vec::with_capacity(1 << 12)));
		}
	}
	fn done(mut self, lower: &[Time], upper: &[Time]) -> Rc<Layer<Key, Val, Time>> {

		let mut count = self.buffer.len();
		for buffer in &self.buffers {
			count += buffer.len();
		}

		let mut builder = <RHHBuilder<Key, Val, Time> as TupleBuilder>::with_capacity(count);

        // sort things, radix if many, `sort` if few.
        if self.buffers.len() > 0 {
        	if self.buffer.len() > 0 {
				self.buffers.push(::std::mem::replace(&mut self.buffer, Vec::new()));
        	}

        	let mut sorter = ::timely_sort::LSBRadixSorter::new();
        	sorter.sort(&mut self.buffers, &|x| (x.0).0.hashed());

        	let mut current_hash = 0;
        	for ((key, val), diff) in self.buffers.drain(..).flat_map(|batch| batch.into_iter()) {
        		if key.hashed().as_u64() != current_hash {
        			current_hash = key.hashed().as_u64();
					consolidate(&mut self.buffer, 0);
					for ((key, val),diff) in self.buffer.drain(..) {
						builder.push_tuple((key, (val, (self.time.clone(), diff))));
					}
        		}
        		self.buffer.push(((key, val), diff));
        	}
			consolidate(&mut self.buffer, 0);
			for ((key, val),diff) in self.buffer.drain(..) {
				builder.push_tuple((key, (val, (self.time.clone(), diff))));
			}
        }
        else {
        	consolidate(&mut self.buffer, 0);
			for ((key, val),diff) in self.buffer.drain(..) {
				builder.push_tuple((key, (val, (self.time.clone(), diff))));
			}
        }

		let layer = builder.done();

		Rc::new(Layer {
			layer: layer,
			desc: Description::new(lower, upper, lower)
		})
	}
}


/// A builder for creating layers from unsorted update tuples.
pub struct OrdBuilder<Key: HashOrdered, Val: Ord, Time: Ord> {
	builder: RHHBuilder<Key, Val, Time>,
}

impl<Key, Val, Time> Builder<Key, Val, Time, Rc<Layer<Key, Val, Time>>> for OrdBuilder<Key, Val, Time> 
where Key: Clone+Default+HashOrdered, Val: Ord+Clone, Time: Lattice+Ord+Clone+Default {

	fn new() -> Self { OrdBuilder { builder: RHHBuilder::new() } }
	fn push(&mut self, (key, val, time, diff): (Key, Val, Time, isize)) {
		self.builder.push_tuple((key, (val, (time, diff))));
	}

	#[inline(never)]
	fn done(self, lower: &[Time], upper: &[Time]) -> Rc<Layer<Key, Val, Time>> {
		Rc::new(Layer {
			layer: self.builder.done(),
			desc: Description::new(lower, upper, lower)
		})
	}
}