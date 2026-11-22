import Lake
import Lake.Load.Workspace

open Lake System

def main (args : List String) : IO Unit := do
  let [root, scratch] := args | throw (IO.userError "expected project and private configuration cache")
  let root := FilePath.mk root
  let (elan?, some lean, some lake) ← findInstall?
    | throw (IO.userError "cannot find pinned Lean/Lake installation")
  let env ← (Env.compute lake lean elan?).toIO IO.userError
  let config : LoadConfig := {
    lakeEnv := env, wsDir := FilePath.mk scratch, pkgDir := root
    configFile := root / defaultConfigFile, reconfigure := true, updateToolchain := false }
  let some resolved ← (resolveConfigFile "[root]" config).toBaseIO
    | throw (IO.userError "cannot resolve clean project configuration")
  let config := resolved.val
  IO.FS.createDirAll config.configDir
  IO.FS.writeFile (config.configDir / "lakefile.olean.trace") ""
  let some workspace ← (loadWorkspaceRoot config).toBaseIO
    | throw (IO.userError "cannot evaluate clean project configuration")
  let package := workspace.root
  unless package.name == `Proofs && package.dir == root &&
      package.buildDir == root / ".lake" / "build" &&
      package.pkgsDir == root / ".lake" / "packages" do
    throw (IO.userError "unsupported first-party output layout")
  IO.println "canonical first-party layout"
