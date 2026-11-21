defmodule KuiperCoordinator.Runners.NamingTest do
  # Ported from kuiper-forge/src/fleet.rs runner_name_* tests (#34, c67bfdd, parity F1)
  use ExUnit.Case, async: true

  alias KuiperCoordinator.Runners.Naming

  test "uses the short hostname" do
    assert Naming.for_host("Jeremys-Mac-mini.local", "abcd1234") == "jeremys-mac-mini-abcd1234"
    assert Naming.for_host("pve01.example.com", "abcd1234") == "pve01-abcd1234"
    assert Naming.for_host("build_box 2", "abcd1234") == "build-box-2-abcd1234"
  end

  test "truncates long hostnames to 20 chars, then trims dashes" do
    assert Naming.for_host("a-really-long-hostname-for-a-build-machine", "abcd1234") ==
             "a-really-long-hostna-abcd1234"

    assert Naming.for_host("a-really-long-host--x", "abcd1234") == "a-really-long-host-abcd1234"
  end

  test "falls back to runner without a usable hostname" do
    assert Naming.for_host("", "abcd1234") == "runner-abcd1234"
    assert Naming.for_host("...", "abcd1234") == "runner-abcd1234"
    assert Naming.for_host(nil, "abcd1234") == "runner-abcd1234"
  end

  test "new/1 adds 8 random hex chars" do
    assert Naming.new("pve01.lan") =~ ~r/\Apve01-[0-9a-f]{8}\z/
    refute Naming.new("pve01") == Naming.new("pve01")
  end
end
