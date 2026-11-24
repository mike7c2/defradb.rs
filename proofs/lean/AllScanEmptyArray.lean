/-!
# AllScanEmptyArray: an `_all` index scan finds the empty arrays

The story, in plain words. An index on an array field stores one entry per
element. A missing field or a null array is stored as a null entry. An empty
array has no element to store, and it used to be stored as a null too, so it
looked like a missing field.

`{tags: {_all: {_eq: 1}}}` asks for documents where every element is 1. An empty
array has no element that breaks that, so it matches (Go's `allSlice` over
nothing is true); a null array does not. The index scan for `_all` looks up the
entries whose value passes the condition, so it never saw the empty arrays.

The fix gives an empty array an entry of its own, the empty marker, and the
`_all` scan also looks the marker up. Every other scan skips the marker: no
element of an empty array passes `_any`, `_eq`, a range or a `_like`.

Who acts: the writer, which turns a document's array into entries, and a scan,
which turns entries back into the documents it hands to the filter.

What must never happen:
- a document that matches `_all` is missing from the `_all` scan (`all_scan_complete`);
- a document the `_all` scan finds has neither a passing element nor an empty
  array, so the scan did more than it needed (`all_scan_sound`);
- a scan other than `_all` hands back an empty array (`value_scan_skips_empty`),
  or `_any` misses one of its matches (`any_scan_complete`);
- a null array matches `_all` (`nil_not_all`);
- two empty arrays clash in a unique index (`empty_arrays_never_clash`);
- with `_all` on two paths of one JSON index, the scan reads one path and the
  marker lookup another, so a match whose scanned array is `[]` is lost
  (`same_path_scan_complete`).

The RED side: with the old writer, which stored `[]` as null, the same scan
misses an empty array that matches (`neg_null_writer_misses_empty`). A scan
that reads one path but looks the marker up at another misses
`{"a": [1], "b": []}` (`neg_split_paths_miss_empty`).

Rust anchors:
- the writer: `IndexManager::expand_value_for_indexing`
  (`crates/database/db/src/index/manager/value_extraction.rs`) and
  `extract_json_leaves` (`crates/building-blocks/document/src/normal.rs`);
- the `_all` scan: `filter_to_index_scan`, `all_path` and `empty_array_scan`
  (`crates/engine/query/src/planner/index_selection/filter_to_scan.rs`);
- the skip: `RangeIterator::next` (`crates/engine/storage/src/index/range_iterator.rs`),
  which drops the marker from every bounded scan, and `full_field_range`, which
  makes the `_ne` and `_like` scans bounded ones;
- the unique exemption: `skips_uniqueness` (`crates/engine/storage/src/index/mod.rs`).
-/

namespace AllScanEmptyArray

-- One index entry of a document's array field. `V` is the element type.
inductive Entry (V : Type) where
  -- a null: the field is missing or the array is null
  | null
  -- the empty marker: the array has no elements
  | empty
  -- one element of the array
  | val (v : V)
  deriving DecidableEq

-- The array of a document: `none` for a null array, `some xs` for the list `xs`.
abbrev Arr (V : Type) := Option (List V)

-- What the writer stores for an array.
def entries {V : Type} : Arr V → List (Entry V)
  -- a null array: one null entry
  | none => [.null]
  -- an empty array: one empty marker
  | some [] => [.empty]
  -- otherwise one entry per element
  | some (x :: xs) => (x :: xs).map .val

-- What the old writer stored: an empty array as a null.
def oldEntries {V : Type} : Arr V → List (Entry V)
  -- a null array: one null entry
  | none => [.null]
  -- an empty array: a null entry, like a missing field
  | some [] => [.null]
  -- otherwise one entry per element
  | some (x :: xs) => (x :: xs).map .val

-- `_all p`: a null array fails, an empty one passes, otherwise every element passes.
def allMatch {V : Type} (p : V → Bool) : Arr V → Bool
  -- a null array never matches
  | none => false
  -- every element, which is none at all for `[]`
  | some xs => xs.all p

-- `_any p`: some element passes.
def anyMatch {V : Type} (p : V → Bool) : Arr V → Bool
  -- a null array never matches
  | none => false
  -- at least one element passes
  | some xs => xs.any p

-- A value scan over `p`: the document has an element entry whose value passes.
-- The null entry and the empty marker are never part of it.
def valueScan {V : Type} (p : V → Bool) (es : List (Entry V)) : Bool :=
  -- look for a passing element entry
  es.any fun e => match e with
    -- an element: keep it when it passes
    | .val v => p v
    -- a null or the empty marker: skipped
    | _ => false

-- Whether an entry is the empty marker.
def isEmpty {V : Type} : Entry V → Bool
  -- the marker
  | .empty => true
  -- a null or an element
  | _ => false

-- The `_all` scan: the value scan, plus a lookup of the empty marker.
def allScan {V : Type} (p : V → Bool) (es : List (Entry V)) : Bool :=
  -- an element that passes, or the marker
  valueScan p es || es.any isEmpty

/-- Every document that matches `_all` is found by the `_all` scan. -/
theorem all_scan_complete {V : Type} (p : V → Bool) (a : Arr V)
    -- the document matches `_all`
    (h : allMatch p a = true) :
    -- then the scan finds it
    allScan p (entries a) = true := by
  -- look at the array
  cases a with
  -- a null array does not match, so this case cannot happen
  | none => simp [allMatch] at h
  | some xs =>
    cases xs with
    -- an empty array: found by the marker lookup
    | nil => simp [allScan, entries, isEmpty]
    -- a non-empty array: its first element passes, so the value scan finds it
    | cons x xs =>
      simp only [allMatch, List.all_cons, Bool.and_eq_true] at h
      simp [allScan, valueScan, entries, h.1]

/-- What the `_all` scan finds has a passing element or is an empty array. -/
theorem all_scan_sound {V : Type} (p : V → Bool) (a : Arr V)
    -- the scan found the document
    (h : allScan p (entries a) = true) :
    -- then it has a passing element, or it is the empty array
    anyMatch p a = true ∨ a = some [] := by
  -- look at the array
  cases a with
  -- a null array has only its null entry, which no part of the scan takes
  | none => simp [allScan, valueScan, entries, isEmpty] at h
  | some xs =>
    cases xs with
    -- the empty array
    | nil => exact Or.inr rfl
    -- a non-empty array has no marker, so an element passed
    | cons x xs =>
      left
      simp only [allScan, entries, Bool.or_eq_true] at h
      rcases h with h | h
      · -- the value scan found a passing element
        simp only [valueScan, List.any_map, List.any_eq_true] at h
        obtain ⟨y, hy, hpy⟩ := h
        simp only [anyMatch, List.any_eq_true]
        exact ⟨y, hy, by simpa using hpy⟩
      · -- element entries are never the marker
        simp [List.any_map, isEmpty] at h

/-- A value scan, the scan of every condition but `_all`, never hands back an
    empty array. -/
theorem value_scan_skips_empty {V : Type} (p : V → Bool) :
    -- the only entry of `[]` is the marker, which the scan skips
    valueScan p (entries (some ([] : List V))) = false := by
  -- one entry, the marker
  rfl

/-- `_any` loses nothing: every document it matches is found by the value scan. -/
theorem any_scan_complete {V : Type} (p : V → Bool) (a : Arr V)
    -- the document matches `_any`
    (h : anyMatch p a = true) :
    -- the value scan finds it
    valueScan p (entries a) = true := by
  -- look at the array
  cases a with
  -- a null array does not match `_any`
  | none => simp [anyMatch] at h
  | some xs =>
    cases xs with
    -- an empty array does not match `_any`
    | nil => simp [anyMatch] at h
    -- otherwise the passing element has its entry
    | cons x xs =>
      simp only [anyMatch, List.any_eq_true] at h
      obtain ⟨y, hy, hpy⟩ := h
      simp only [valueScan, entries, List.any_map, List.any_eq_true]
      exact ⟨y, hy, by simpa using hpy⟩

/-- A null array never matches `_all`. -/
theorem nil_not_all {V : Type} (p : V → Bool) :
    -- Go's `allSlice` over a nil slice is false
    allMatch p (none : Arr V) = false := by
  -- by the definition
  rfl

/-- An empty array always matches `_all`. -/
theorem empty_all {V : Type} (p : V → Bool) :
    -- nothing breaks "every element passes"
    allMatch p (some ([] : List V)) = true := by
  -- by the definition
  rfl

-- Two documents clash in a unique index when they share an entry that is a real
-- value. Null entries and the empty marker never clash, as `skips_uniqueness` says.
def clash {V : Type} [DecidableEq V] (a b : List (Entry V)) : Bool :=
  -- a shared element entry
  a.any fun e => match e with
    -- an element shared by both
    | .val v => b.contains (.val v)
    -- a null or the marker: never a clash
    | _ => false

/-- Two empty arrays can live in the same unique index. -/
theorem empty_arrays_never_clash {V : Type} [DecidableEq V] :
    -- both have only the marker, which never clashes
    clash (entries (some ([] : List V))) (entries (some ([] : List V))) = false := by
  -- one entry each, the marker
  rfl

/-- RED: with the old writer, an empty array is a null entry, so the `_all` scan
    misses it even though it matches. -/
theorem neg_null_writer_misses_empty :
    -- `[]` matches `_all (= 1)`
    allMatch (fun n : Nat => n == 1) (some []) = true
    -- but the scan over the old entries does not find it
    ∧ allScan (fun n : Nat => n == 1) (oldEntries (some [])) = false := by
  -- both sides are literal; Lean computes them
  decide

-- A document with arrays at two paths of one JSON field, `a` and `b`.
structure TwoPaths (V : Type) where
  -- the array at path `a`
  a : Arr V
  -- the array at path `b`
  b : Arr V

-- `{a: {_all: p}, b: {_all: q}}`: both arrays pass their `_all`.
def bothAll {V : Type} (p q : V → Bool) (d : TwoPaths V) : Bool :=
  -- each path checked on its own
  allMatch p d.a && allMatch q d.b

-- The shipped scan: it reads path `a` and looks the marker up at `a` too.
def samePathScan {V : Type} (p : V → Bool) (d : TwoPaths V) : Bool :=
  -- the `_all` scan of one path, as `allScan` above
  allScan p (entries d.a)

-- The broken scan: it reads path `b` but looks the marker up at path `a`.
def splitPathScan {V : Type} (q : V → Bool) (d : TwoPaths V) : Bool :=
  -- elements of `b` that pass `b`'s condition, or the marker of `a`
  valueScan q (entries d.b) || (entries d.a).any isEmpty

/-- When the scan and the marker lookup share a path, every document matching
    `_all` on both paths is found; the filter checks the other path. -/
theorem same_path_scan_complete {V : Type} (p q : V → Bool) (d : TwoPaths V)
    -- the document matches both conditions
    (h : bothAll p q d = true) :
    -- the scan finds it
    samePathScan p d = true := by
  -- the condition on `a` holds on its own
  simp only [bothAll, Bool.and_eq_true] at h
  -- and one path is the single-path case
  exact all_scan_complete p d.a h.1

/-- RED: scanning `b` while looking the marker up at `a` misses
    `{"a": [1], "b": []}`, which matches `_all (≠ 25)` on both paths. -/
theorem neg_split_paths_miss_empty :
    -- the document matches
    bothAll (fun n : Nat => n != 25) (fun n : Nat => n != 25) ⟨some [1], some []⟩ = true
    -- but the split scan does not find it
    ∧ splitPathScan (fun n : Nat => n != 25) ⟨some [1], some []⟩ = false := by
  -- both sides are literal; Lean computes them
  decide

end AllScanEmptyArray

#print axioms AllScanEmptyArray.all_scan_complete
#print axioms AllScanEmptyArray.all_scan_sound
#print axioms AllScanEmptyArray.value_scan_skips_empty
#print axioms AllScanEmptyArray.any_scan_complete
#print axioms AllScanEmptyArray.nil_not_all
#print axioms AllScanEmptyArray.empty_all
#print axioms AllScanEmptyArray.empty_arrays_never_clash
#print axioms AllScanEmptyArray.neg_null_writer_misses_empty
#print axioms AllScanEmptyArray.same_path_scan_complete
#print axioms AllScanEmptyArray.neg_split_paths_miss_empty
