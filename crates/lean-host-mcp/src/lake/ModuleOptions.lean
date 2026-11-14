import Lake
import Lake.Load.Workspace
import Lean.Language.Lean

open Lean Lake System

-- Use Lean's own parser for -D arguments, never guess the type of an option.
def commandOptions (args : Array String) : IO LeanOptions := do
  let mut options := Options.empty
  let mut needsValue := false
  for arg in args do
    if !needsValue && (arg == "-D" || arg == "--define") then
      needsValue := true
      continue
    let assignment ← if needsValue then pure arg
      else if arg.startsWith "-D" then pure (arg.drop 2).toString
      else if arg.startsWith "--define=" then pure (arg.drop 9).toString
      else throw <| IO.userError s!"unsupported extra Lean compiler argument: {arg}"
    needsValue := false
    let parts := assignment.splitOn "="
    if parts.length < 2 then
      throw <| IO.userError "invalid -D argument: expected name=value"
    let name := parts.head!.toName
    let value := String.intercalate "=" parts.tail
    if let some decl := (← getOptionDecls).find? name then
      options ← Language.Lean.setOption options decl name value
    else
      -- Like Lean's CLI, defer options registered by project imports until elaboration.
      options := options.set name value
  if needsValue then
    throw <| IO.userError "missing value after -D/--define"
  let some result := LeanOptions.fromOptions? options
    | throw <| IO.userError "unsupported Lean option value type"
  return result

-- Load configuration only. Never enter Lake's build graph or materialize dependencies.
def main (args : List String) : IO UInt32 := do
  let [workspace, root, configFile, dependency, configWorkspace, file] := args | throw <| IO.userError "expected workspace, package root, config file, dependency name and source file"
  let root ← IO.FS.realPath root
  let (elan?, some lean, some lake) ← findInstall?
    | throw <| IO.userError "cannot find pinned Lean/Lake installation"
  let env ← (Env.compute lake lean elan?).toIO IO.userError
  let lakeOpts ← if dependency.isEmpty then pure {} else do
    let workspace ← IO.FS.realPath workspace
    let some parent ← (loadWorkspaceRoot {lakeEnv := env, wsDir := workspace, updateToolchain := false}).toBaseIO
      | throw <| IO.userError "cannot load parent Lake configuration"
    let some dep := parent.root.depConfigs.find? (fun dep => dep.name.toString (escape := false) == dependency)
      | throw <| IO.userError s!"source checks for transitive dependency {dependency} are unsupported: parent configuration options cannot be determined without full workspace loading"
    pure dep.opts
  -- Dependency configuration must not inherit a standalone package's saved -K
  -- values, nor overwrite a live build's config cache. Only the cache directory
  -- moves; configuration source and __dir__ remain in the real package.
  let configRoot := if dependency.isEmpty then root else FilePath.mk configWorkspace
  let some ws ← (loadWorkspaceRoot {
      lakeEnv := env, wsDir := configRoot, pkgDir := root,
      relConfigFile := configFile, configFile := root / configFile,
      lakeOpts, updateToolchain := false }).toBaseIO
    | throw <| IO.userError "cannot load Lake package configuration"
  let mod? ← if file.isEmpty then pure none else do
    pure <| ws.findModuleBySrc? (← IO.FS.realPath file)
  let (moduleName, options, compilerArgs) := match mod? with
    | some mod => (mod.name.toString, mod.leanOptions, mod.weakLeanArgs ++ mod.leanArgs)
    | none => ("", ws.leanOptions, ws.root.weakLeanArgs ++ ws.leanArgs)
  -- Lean's frontend gives setup options precedence over command-line -D options.
  let options := (← commandOptions compilerArgs) ++ options
  IO.println <| (Json.mkObj [
    ("module", toJson moduleName),
    ("options", toJson options)]).compress
  return 0
