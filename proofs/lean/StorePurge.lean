/-!
# StorePurge: what a store purge drops and what it removes

The story, in plain words. A purge wipes a node's store and starts the node
again. The wipe is handed a target: the store's data directory, which it
opens and drops every key of (`drop_all`), when the store is on disk; the
directories the node owns besides the store, which it removes (the embedded
Vera node's chain and journal); and the paths it must keep, the keyring
first among them.

What must never happen: the data directory is removed (it is emptied, never
deleted), a path that is not one of the owned directories is removed, a
removal reaches the keyring (removing it, or a directory above it), or a
memory store, which has nothing on disk, gets something dropped.

The wipe checks its target before touching anything (`plan`): when an owned
directory is the data directory, the keyring or a directory above either, it
refuses, and nothing is dropped or removed. This file proves, for every
target the check lets through:

* the removal set is exactly the owned directories (`removal_is_owned_dirs`);
* the data directory is the drop target and is never in the removal set,
  nor is any directory above it (`data_dir_is_dropped_never_removed`);
* no removed path equals or sits above a kept path such as the keyring
  (`removal_never_reaches_kept`);
* a memory target drops nothing and still removes exactly its owned
  directories (`memory_drops_nothing`);
* the layout `defradb start` builds is let through
  (`cli_layout_is_accepted`).

The order of the purge's steps is the TLA+ model `proofs/tla/StorePurge.tla`.

Rust anchors: `crates/engine/storage/src/wipe.rs` `WipeTarget`, `wipe`,
`check`; `crates/entry-points/cli/src/commands/start/purge.rs` `wipe_target`.
-/

namespace StorePurge

-- A path is its list of components, so "sits above" is "is a prefix of".
abbrev Path := List String

-- What a wipe is handed.
structure Target where
  -- The data directory to drop every key of; none for a memory store.
  drop : Option Path
  -- The directories to remove.
  owned : List Path
  -- The paths that must survive: the keyring.
  kept : List Path

-- The paths no owned directory may equal or sit above: the kept paths and
-- the data directory.
def protectedPaths (t : Target) : List Path := t.kept ++ t.drop.toList

-- The check the wipe runs first: no owned directory is, or sits above, a
-- protected path.
def accepted (t : Target) : Bool :=
  t.owned.all fun dir => (protectedPaths t).all fun keep => !dir.isPrefixOf keep

-- What the wipe does with a target: the drop target and the removal set it
-- acts on, or nothing at all when the check refuses the target.
def plan (t : Target) : Option (Option Path × List Path) :=
  if accepted t then some (t.drop, t.owned) else none

-- The removal set is exactly the owned directories.
theorem removal_is_owned_dirs (t : Target) (d : Option Path) (r : List Path)
    (h : plan t = some (d, r)) : r = t.owned := by
  -- Only the accepted branch answers, and it answers the owned list.
  unfold plan at h
  split at h
  · -- Accepted: the pair is (drop, owned).
    cases h
    rfl
  · -- Refused: no plan, so this case cannot happen.
    cases h

-- An accepted target says nothing removed equals or sits above a protected
-- path.
theorem accepted_spares (t : Target) (h : accepted t = true) :
    ∀ dir ∈ t.owned, ∀ keep ∈ protectedPaths t, dir.isPrefixOf keep = false := by
  -- Unfold the two nested `all`s into their membership statements.
  intro dir hdir keep hkeep
  unfold accepted at h
  rw [List.all_eq_true] at h
  have hd := h dir hdir
  rw [List.all_eq_true] at hd
  have hk := hd keep hkeep
  -- `!b = true` is `b = false`.
  simpa using hk

-- The data directory is what is dropped, and neither it nor any directory
-- above it is removed.
theorem data_dir_is_dropped_never_removed (t : Target) (d : Option Path)
    (r : List Path) (h : plan t = some (d, r)) :
    d = t.drop ∧ ∀ data ∈ t.drop, ∀ dir ∈ r, dir.isPrefixOf data = false := by
  -- Take the accepted branch apart, then use the check on the data path.
  unfold plan at h
  split at h
  · rename_i hacc
    cases h
    refine ⟨rfl, ?_⟩
    intro data hdata dir hdir
    -- The data directory is one of the protected paths.
    have hp : data ∈ protectedPaths t := by
      unfold protectedPaths
      exact List.mem_append_right _ (Option.mem_toList.mpr hdata)
    exact accepted_spares t hacc dir hdir data hp
  · cases h

-- No removed path equals or sits above a kept path, such as the keyring.
theorem removal_never_reaches_kept (t : Target) (d : Option Path) (r : List Path)
    (h : plan t = some (d, r)) : ∀ dir ∈ r, ∀ keep ∈ t.kept, dir.isPrefixOf keep = false := by
  -- The kept paths are protected, and the accepted branch removes only owned ones.
  unfold plan at h
  split at h
  · rename_i hacc
    cases h
    intro dir hdir keep hkeep
    have hp : keep ∈ protectedPaths t := by
      unfold protectedPaths
      exact List.mem_append_left _ hkeep
    exact accepted_spares t hacc dir hdir keep hp
  · cases h

-- A memory target drops nothing and removes exactly its owned directories.
theorem memory_drops_nothing (t : Target) (hmem : t.drop = none) (d : Option Path)
    (r : List Path) (h : plan t = some (d, r)) : d = none ∧ r = t.owned := by
  -- The drop target is the target's own, which is none.
  have hd := (data_dir_is_dropped_never_removed t d r h).1
  exact ⟨hd.trans hmem, removal_is_owned_dirs t d r h⟩

-- The target `defradb start` builds under root directory `root`: the data
-- directory `root/data` when the store is on disk, the Vera directories
-- `root/vera` and `root/vera-worker` when the embedded Vera node runs, and
-- the keyring `root/keys` kept.
def cliTarget (root : Path) (persistent vera : Bool) : Target where
  drop := if persistent then some (root ++ ["data"]) else none
  owned := if vera then [root ++ ["vera"], root ++ ["vera-worker"]] else []
  kept := [root ++ ["keys"]]

-- The default layout is accepted, for each store kind and Vera setting.
theorem cli_layout_is_accepted (persistent vera : Bool) :
    accepted (cliTarget ["home", "defradb"] persistent vera) = true := by
  -- Four concrete cases, each decided by computing the check.
  cases persistent <;> cases vera <;> decide

end StorePurge
