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

-- Lake's public loader normally writes its configuration cache. Keep every
-- framework-managed cache write private while preserving the real pkgDir,
-- configuration source, cwd and __dir__. In the pinned Lake implementation,
-- a pre-existing private trace with reconfigure=true bypasses the cold branch
-- which would otherwise create pkgDir/.lake, even with a private wsDir.
def freshWorkspace (cfg : LoadConfig) : IO Workspace := do
  let some resolved ← (resolveConfigFile "[root]" {cfg with reconfigure := true}).toBaseIO
    | throw <| IO.userError "cannot resolve Lake package configuration"
  let cfg := resolved.val
  if cfg.configLang? == some .lean then
    let some configName := FilePath.mk <$> cfg.configFile.fileName
      | throw <| IO.userError "invalid Lake configuration filename"
    IO.FS.createDirAll cfg.configDir
    IO.FS.writeFile (cfg.configDir / configName.withExtension "olean.trace") ""
  let some ws ← (loadWorkspaceRoot cfg).toBaseIO
    | throw <| IO.userError "cannot evaluate Lake package configuration"
  return ws

-- Preserve only the root's saved -K settings. Dependency settings come from
-- the freshly evaluated parent's `require ... with`, never a standalone cache.
-- This bounded read runs in the project's read turn, so no build the server
-- runs changes the file, and it neither imports nor modifies the original
-- cached configuration.
def savedRootLakeOptions (cfg : LoadConfig) : IO (NameMap String) := do
  let some resolved ← (resolveConfigFile "[root]" cfg).toBaseIO
    | throw <| IO.userError "cannot resolve root Lake package configuration"
  let cfg := resolved.val
  if cfg.configLang? != some .lean then return {}
  let some configName := FilePath.mk <$> cfg.configFile.fileName
    | throw <| IO.userError "invalid root Lake configuration filename"
  let trace := cfg.configDir / configName.withExtension "olean.trace"
  if !(← trace.pathExists) then return {}
  if (← trace.symlinkMetadata).type != .file then
    throw <| IO.userError "saved root Lake configuration trace is not a regular file"
  let input ← IO.FS.Handle.mk trace .read
  let limit := 256 * 1024
  let mut bytes := ByteArray.empty
  repeat
    let chunk ← input.read (USize.ofNat (limit + 1 - bytes.size))
    if chunk.isEmpty then break
    bytes := bytes ++ chunk
    if bytes.size > limit then
      throw <| IO.userError "saved root Lake options exceed 256 KiB"
  let some text := String.fromUTF8? bytes
    | throw <| IO.userError "saved root Lake options are not UTF-8"
  let .ok options := Json.parse text >>= fun json => JsonObject.fromJson? json >>= (·.get "options")
    | throw <| IO.userError "saved root Lake configuration trace has invalid options"
  return options

-- Evaluate configuration afresh, without entering Lake's build graph,
-- materializing dependencies, or using the project's compiled config cache.
def main (args : List String) : IO UInt32 := do
  let [workspace, root, configFile, dependency, configWorkspace, file] := args
    | throw <| IO.userError "expected workspace, package root, config file, dependency name, private workspace and source file"
  let root ← IO.FS.realPath root
  let workspace ← IO.FS.realPath workspace
  let (elan?, some lean, some lake) ← findInstall?
    | throw <| IO.userError "cannot find pinned Lean/Lake installation"
  let env ← (Env.compute lake lean elan?).toIO IO.userError
  let rootConfig : LoadConfig := {
    lakeEnv := env, wsDir := workspace, updateToolchain := false }
  let savedOptions ← savedRootLakeOptions rootConfig
  let privateRoot := FilePath.mk configWorkspace
  let lakeOpts ← if dependency.isEmpty then pure savedOptions else do
    let parent ← freshWorkspace {rootConfig with
      wsDir := privateRoot / "parent", pkgDir := workspace,
      configFile := workspace / defaultConfigFile, lakeOpts := savedOptions }
    let some dep := parent.root.depConfigs.find? (fun dep => dep.name.toString (escape := false) == dependency)
      | throw <| IO.userError s!"source checks for transitive dependency {dependency} are unsupported: parent configuration options cannot be determined without full workspace loading"
    pure dep.opts
  let ws ← freshWorkspace {
    lakeEnv := env, wsDir := privateRoot / "package", pkgDir := root,
    relConfigFile := configFile, configFile := root / configFile,
    lakeOpts, updateToolchain := false }
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
