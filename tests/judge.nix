# A VM run's check (tests/record.py): the run records and always builds, so its evidence stays in the store; this
# fails on the run's verdict and names that path. `.run` on the check gives the run itself, built already: after
# a failure, `nix build .#checks.x86_64-linux.<check>.run --print-out-paths` prints the evidence without a rerun.
{pkgs}: run:
pkgs.runCommand "${run.name}-check" {passthru = {inherit run;};} ''
  if [ -s ${run}/failed ]; then
    echo "${run.name} failed:"
    cat ${run}/failed
    echo "evidence: ${run}, a directory per machine with its journal and files; the VM consoles: nix log ${run}"
    exit 1
  fi
  touch $out
''
