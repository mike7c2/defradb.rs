import OrderEncoding.BytesEnc
import OrderEncoding.CrossType

/-!
# OrderEncoding.JsonPath: a JSON index key keeps its path ascending

The story, in plain words. An index on a JSON field stores one key per leaf:
the path to the leaf, then the leaf's value. A path is a list of steps, each a
property name or "some array element". A descending index must sort values the
other way round, so it flips the value's bytes. It used to flip the path's bytes
too. A flipped array step (byte 136) reads as a flipped letter `w`, so a reader
could not tell one from the other and `_any` on a descending JSON array failed
(issue 3143). Go never flips the path, only the value.

Who acts: the encoder, which writes `JSON marker, path, terminator, value`, and
the reader, which splits a key back into path and value.

What must never happen:
- two different paths giving keys that start the same way, so the reader cannot
  tell which path a key belongs to (`decodePath_encodePath`, `encodePath_injective`);
- in a descending index, the values at one path coming out in the wrong order
  (`desc_key_order`);
- the scan bounds of a path missing a value at that path, or catching a value at
  a longer path (`sentinels_bound_the_path`).

Each step now starts with a byte of its own: a property with the bytes marker 6,
as Go writes it, and an array step with 136. Without that byte two different
paths can give the very same bytes (`neg_untagged_paths_collide`).

Rust anchors (`crates/engine/storage/src/encoding/json.rs`):
- `encode_json`, `encode_path`: the path is written ascending in both directions;
- `decode_path`: the reader, the same for both directions. It also reads the
  `PathMax` end bytes `1 48`; no step starts with 1 (`encPart_head`), so that
  never cuts a path short;
- `encode_json_descending`: flips only the value.
Range bounds: `path_low_end`, `path_high_end` in
`crates/engine/query/src/planner/index_selection/filter_to_scan.rs`.
-/

namespace OrderEncoding

namespace JsonPath

-- One step of a JSON path.
inductive Part where
  -- a property name, given as its bytes
  | prop (name : Bytes)
  -- some element of an array (Go writes every position as 0)
  | index
  deriving DecidableEq

-- `JSON_MARKER`: the first byte of every JSON index key.
def mJson : Nat := 11
-- The byte an array step is written as: the integer 0, which is `INT_ZERO`.
def mIndex : Nat := 136
-- `ESCAPED_TERM`: the first byte after the last step.
def mTerm : Nat := 1
-- `JSON_PATH_END` (`/`): the second byte after the last step.
def mPathEnd : Nat := 47

-- How one step is written: a property is the bytes marker, then its escaped
-- bytes and their terminator; an array step is the single byte 136.
def encPart : Part → Bytes
  -- `encode_bytes_ascending(name)`
  | .prop name => mBytes :: encBytesBody name
  -- `encode_uvarint_ascending(0)`
  | .index => [mIndex]

-- How a whole path is written: each step in turn, then the two end bytes.
def encPath : List Part → Bytes
  -- no step left: the terminator and the path end
  | [] => [mTerm, mPathEnd]
  -- a step, then the rest of the path
  | p :: ps => encPart p ++ encPath ps

-- Read one escaped name and its terminator back, returning the name and what
-- follows it. `none` means the bytes are not a written name.
def decBody : Bytes → Option (Bytes × Bytes)
  -- the terminator `0 1`: the name ended
  | 0 :: 1 :: rest => some ([], rest)
  -- the escape `0 255`: a zero byte of the name
  | 0 :: 255 :: rest => (decBody rest).map (fun r => (0 :: r.1, r.2))
  -- any other zero: not something the encoder writes
  | 0 :: _ => none
  -- a plain byte of the name
  | c :: rest => (decBody rest).map (fun r => (c :: r.1, r.2))
  -- ran out of bytes before the terminator
  | [] => none

-- Reading a written name gives the name back, and leaves what followed it.
theorem decBody_encBytesBody : ∀ (name rest : Bytes),
    -- every byte of the name is a byte
    (∀ b ∈ name, b ≤ 255) →
    -- reading the written name and what follows gives both back
    decBody (encBytesBody name ++ rest) = some (name, rest) := by
  -- one byte of the name at a time
  intro name
  induction name with
  -- the empty name is just the terminator
  | nil => intro rest _; rfl
  | cons c cs ih =>
    intro rest hb
    -- the rest of the name is made of bytes too
    have hcs : ∀ b ∈ cs, b ≤ 255 := fun b h => hb b (List.mem_cons_of_mem c h)
    -- split on whether this byte is the escaped zero
    by_cases hc : c = 0
    · -- a zero is written `0 255`, read back as a zero
      subst hc
      simp [encBytesBody, encByte, decBody, ih rest hcs]
    · -- any other byte is written as itself, read back as itself
      have hpos : c = (c - 1) + 1 := by omega
      rw [hpos]
      simp [encBytesBody, encByte, decBody, ih rest hcs]

-- Read a path back, step by step, for at most `fuel` steps.
def decPath : Nat → Bytes → Option (List Part × Bytes)
  -- out of fuel: give up
  | 0, _ => none
  -- the two end bytes: the path is over
  | _ + 1, 1 :: 47 :: rest => some ([], rest)
  -- the bytes marker: a property name follows
  | n + 1, 6 :: rest =>
    match decBody rest with
    -- the name, then the rest of the path
    | some (name, r) => (decPath n r).map (fun t => (.prop name :: t.1, t.2))
    -- not a written name
    | none => none
  -- the array byte: an array step
  | n + 1, 136 :: rest => (decPath n rest).map (fun t => (.index :: t.1, t.2))
  -- anything else does not start a step
  | _ + 1, _ => none

-- A path is well formed when every property name is made of bytes.
def Wf : List Part → Prop
  -- the empty path
  | [] => True
  -- a property: its bytes are bytes, and the rest is well formed
  | .prop name :: ps => (∀ b ∈ name, b ≤ 255) ∧ Wf ps
  -- an array step: the rest is well formed
  | .index :: ps => Wf ps

/-- Reading a written path gives the path back, whatever follows it, given
    enough fuel for its steps. -/
theorem decodePath_encodePath : ∀ (path : List Part) (rest : Bytes) (n : Nat),
    -- the path is well formed
    Wf path →
    -- the fuel covers every step and the end
    path.length < n →
    -- reading gives the path and what follows it
    decPath n (encPath path ++ rest) = some (path, rest) := by
  -- one step at a time
  intro path
  induction path with
  | nil =>
    -- the empty path is the two end bytes
    intro rest n _ hn
    -- some fuel is left
    obtain ⟨m, rfl⟩ : ∃ m, n = m + 1 := ⟨n - 1, by simp at hn; omega⟩
    rfl
  | cons p ps ih =>
    intro rest n hwf hn
    -- one unit of fuel for this step, the rest for the others
    obtain ⟨m, rfl⟩ : ∃ m, n = m + 1 := ⟨n - 1, by simp at hn; omega⟩
    have hm : ps.length < m := by simp at hn; omega
    cases p with
    | prop name =>
      -- a property: its name is read back, then the rest of the path
      obtain ⟨hname, hps⟩ := hwf
      have hbody := decBody_encBytesBody name (encPath ps ++ rest) hname
      simp only [encPath, encPart, List.cons_append, List.append_assoc]
      simp [decPath, mBytes, hbody, ih rest m hps hm]
    | index =>
      -- an array step: one byte, then the rest of the path
      simp only [encPath, encPart, List.cons_append, List.nil_append]
      simp [decPath, mIndex, ih rest m hwf hm]

/-- Two well-formed paths whose keys are equal, after the path or anywhere
    later, are the same path, followed by the same bytes. -/
theorem encodePath_injective (p q : List Part) (x y : Bytes)
    -- both paths are well formed
    (hp : Wf p) (hq : Wf q)
    -- their keys are equal
    (h : encPath p ++ x = encPath q ++ y) :
    -- then the paths and what follows are equal
    p = q ∧ x = y := by
  -- read both keys with the same fuel, enough for either path
  have ep := decodePath_encodePath p x (p.length + q.length + 1) hp (by omega)
  have eq := decodePath_encodePath q y (p.length + q.length + 1) hq (by omega)
  -- the same bytes read the same way
  rw [h] at ep
  rw [ep] at eq
  -- so the two results are equal
  simp at eq
  exact eq

-- Comparing two keys that share a start comes down to what follows it.
theorem lex_append_left : ∀ (pre a b : Bytes), (pre ++ a) <ₗ (pre ++ b) ↔ a <ₗ b := by
  -- one shared byte at a time
  intro pre
  induction pre with
  -- nothing shared: the keys are `a` and `b`
  | nil => intro a b; rfl
  -- a shared first byte is skipped
  | cons c cs ih => intro a b; simp only [List.cons_append]; rw [lex_cons_iff]; exact ih a b

-- The key of a value at a path: the JSON marker, the path, then the value's bytes.
def key (path : List Part) (value : Bytes) : Bytes := mJson :: (encPath path ++ value)

/-- In a descending index the values at one path come back in reverse order.
    `enc` is how a descending index writes a value; all we need of it is that it
    reverses the order of values. -/
theorem desc_key_order {V : Type} (lt : V → V → Prop) (enc : V → Bytes)
    -- the descending value encoding reverses the order of values
    (hdesc : ∀ v w, lt v w ↔ enc w <ₗ enc v)
    -- any path and two values
    (path : List Part) (v w : V) :
    -- the key of `w` sorts first exactly when `v` is the smaller value
    key path (enc w) <ₗ key path (enc v) ↔ lt v w := by
  -- the marker and the path are shared, so the values decide
  unfold key
  rw [lex_cons_iff, lex_append_left]
  exact (hdesc v w).symm

-- The low bound of a path: the end bytes, then a zero (`PathMin`).
def pathMin (path : List Part) : Bytes := key path [0]
-- The high bound of a path: the terminator, then the byte after the path end (`PathMax`).
def pathMax (path : List Part) : Bytes := mJson :: (encPathOpen path ++ [mTerm, mPathEnd + 1])
where
  -- the path's steps without the two end bytes
  encPathOpen : List Part → Bytes
    -- no step: nothing
    | [] => []
    -- a step, then the others
    | p :: ps => encPart p ++ encPathOpen ps

-- A written path is its steps and then the two end bytes.
theorem encPath_eq_open (path : List Part) :
    encPath path = pathMax.encPathOpen path ++ [mTerm, mPathEnd] := by
  -- one step at a time
  induction path with
  -- no step: both are the two end bytes
  | nil => rfl
  -- a step: the same step in front of both
  | cons p ps ih => simp [encPath, pathMax.encPathOpen, ih]

/-- The two bounds hold every value at the path, ascending or descending: a
    value's bytes never start with anything below zero, and the high bound's
    last byte is above the path end. -/
theorem sentinels_bound_the_path (path : List Part) (value : Bytes)
    -- every encoded value has at least its marker byte
    (hne : value ≠ []) :
    -- at or above the low bound
    (pathMin path = key path value ∨ pathMin path <ₗ key path value)
    -- strictly below the high bound
    ∧ key path value <ₗ pathMax path := by
  -- the value's first byte is `m`
  obtain ⟨m, vs, rfl⟩ : ∃ m vs, value = m :: vs := by
    cases value with
    | nil => exact absurd rfl hne
    | cons m vs => exact ⟨m, vs, rfl⟩
  constructor
  · -- the shared start is skipped; then `[0]` against `m :: vs`
    unfold pathMin key
    by_cases hm : m = 0
    · subst hm
      cases vs with
      -- the value is exactly the zero byte: equal to the bound
      | nil => exact Or.inl rfl
      -- a longer value starting with zero: the bound is a prefix, so below
      | cons v vs =>
        right
        rw [lex_cons_iff, lex_append_left]
        exact Lex.cons Lex.nil
    · -- a first byte above zero: the bound is below
      right
      rw [lex_cons_iff, lex_append_left]
      exact Lex.head (by omega)
  · -- after the steps, the key has `1 47 ...` and the bound has `1 48`
    unfold key pathMax
    rw [lex_cons_iff, encPath_eq_open, List.append_assoc, lex_append_left]
    exact Lex.cons (Lex.head (by decide))

-- The steps of a longer path start with the steps of the shorter one.
theorem encPathOpen_append (p q : List Part) :
    pathMax.encPathOpen (p ++ q) = pathMax.encPathOpen p ++ pathMax.encPathOpen q := by
  -- one step of `p` at a time
  induction p with
  -- `p` is empty: nothing in front
  | nil => rfl
  -- a step of `p` in front of both
  | cons a as ih => simp [pathMax.encPathOpen, ih]

-- Every step starts with a byte above the terminator: 6 for a property, 136
-- for an array step. So the reader never takes a step for the end bytes.
theorem encPart_head (step : Part) : ∃ b bs, encPart step = b :: bs ∧ mTerm < b := by
  cases step with
  -- a property starts with 6
  | prop name => exact ⟨mBytes, encBytesBody name, rfl, by decide⟩
  -- an array step is 136
  | index => exact ⟨mIndex, [], rfl, by decide⟩

/-- A key at a longer path is above the high bound of the shorter one. -/
theorem child_key_above_pathMax (path more : List Part) (step : Part) (value : Bytes) :
    -- the bound of `path` is below any key whose path has `step` after `path`
    pathMax path <ₗ key (path ++ step :: more) value := by
  -- skip the marker and the shared steps
  unfold key pathMax
  rw [lex_cons_iff, encPath_eq_open, encPathOpen_append, List.append_assoc,
    List.append_assoc, lex_append_left]
  -- what is left: `1 48` against the step's first byte, which is above 1
  obtain ⟨b, bs, hb, hlt⟩ := encPart_head step
  simp only [pathMax.encPathOpen, hb, List.cons_append]
  exact Lex.head hlt

-- The layout before this change: a property written without the bytes marker.
def encPartUntagged : Part → Bytes
  -- the escaped name alone
  | .prop name => encBytesBody name
  -- the array byte, as now
  | .index => [mIndex]

-- A whole path in that layout.
def encPathUntagged : List Part → Bytes
  -- the two end bytes, as now
  | [] => [mTerm, mPathEnd]
  -- each step in turn
  | p :: ps => encPartUntagged p ++ encPathUntagged ps

/-- RED: without the bytes marker, the path "array step, then property `5`" and
    the path "property `136 5`" are written as the very same bytes, so the reader
    cannot tell them apart. -/
theorem neg_untagged_paths_collide :
    -- two different paths
    ([Part.index, Part.prop [5]] ≠ [Part.prop [136, 5]])
    -- with the same bytes
    ∧ encPathUntagged [Part.index, Part.prop [5]] = encPathUntagged [Part.prop [136, 5]] := by
  -- both sides are literal; Lean computes them
  decide

end JsonPath

end OrderEncoding
