defmodule KuiperCoordinator.AgentUpdatesTest do
  # Ported from the Rust coordinator's agent_updates.rs tests (parity U1, #75).
  use ExUnit.Case, async: false

  alias KuiperCoordinator.AgentUpdates

  @releases [
    %{
      "tag_name" => "kuiper-proxmox-agent-v0.6.0-rc.1",
      "html_url" => "u-rc",
      "draft" => false,
      "prerelease" => false,
      "published_at" => nil
    },
    %{
      "tag_name" => "kuiper-proxmox-agent-v0.5.1",
      "html_url" => "u-draft",
      "draft" => true,
      "prerelease" => false,
      "published_at" => nil
    },
    %{
      "tag_name" => "kuiper-libvirt-agent-v0.2.0",
      "html_url" => "u-libvirt",
      "draft" => false,
      "prerelease" => false,
      "published_at" => "2026-10-30T13:38:55Z"
    },
    %{
      "tag_name" => "kuiper-proxmox-agent-v0.5.0",
      "html_url" => "u-pve-050",
      "draft" => false,
      "prerelease" => false,
      "published_at" => "2026-10-30T13:38:41Z"
    },
    %{
      "tag_name" => "kuiper-agent-lib-v0.6.0",
      "html_url" => "u-lib",
      "draft" => false,
      "prerelease" => false,
      "published_at" => nil
    },
    %{
      "tag_name" => "kuiper-forge-v0.8.0",
      "html_url" => "u-forge",
      "draft" => false,
      "prerelease" => false,
      "published_at" => nil
    },
    %{
      "tag_name" => "kuiper-proxmox-agent-v0.4.3",
      "html_url" => "u-pve-043",
      "draft" => false,
      "prerelease" => false,
      "published_at" => nil
    },
    %{
      "tag_name" => "kuiper-tart-agent-v0.5.10",
      "html_url" => "u-tart-510",
      "draft" => false,
      "prerelease" => false,
      "published_at" => nil
    },
    %{
      "tag_name" => "kuiper-tart-agent-v0.5.9",
      "html_url" => "u-tart-59",
      "draft" => false,
      "prerelease" => false,
      "published_at" => nil
    },
    %{
      "tag_name" => "nightly",
      "html_url" => "u-n",
      "draft" => false,
      "prerelease" => true,
      "published_at" => nil
    }
  ]

  setup do
    AgentUpdates.set(%{})
    on_exit(fn -> AgentUpdates.set(%{}) end)
  end

  defp release(tag),
    do: [%{"tag_name" => tag, "html_url" => "u-#{tag}", "draft" => false, "prerelease" => false}]

  # pages[n] is served for ?page=n+1 (page 1 without a param), with a Link rel="next" on every
  # page but the last
  defp stub_github(pages) do
    name = :"github_#{System.unique_integer([:positive])}"

    Req.Test.stub(name, fn conn ->
      conn = Plug.Conn.fetch_query_params(conn)
      page = String.to_integer(conn.query_params["page"] || "1")
      {status, body} = Enum.at(pages, page - 1)

      conn =
        if page < length(pages),
          do:
            Plug.Conn.put_resp_header(
              conn,
              "link",
              ~s(<http://github.test/releases?per_page=100&page=#{page + 1}>; rel="next")
            ),
          else: conn

      conn |> Plug.Conn.put_status(status) |> Req.Test.json(body)
    end)

    [plug: {Req.Test, name}, retry: false]
  end

  test "newest agent releases skip drafts, prereleases and other crates; semver order" do
    newest = AgentUpdates.newest_agent_releases(@releases)

    assert Enum.sort(Map.keys(newest)) ==
             ~w(kuiper-libvirt-agent kuiper-proxmox-agent kuiper-tart-agent)

    assert newest["kuiper-proxmox-agent"].url == "u-pve-050"
    assert to_string(newest["kuiper-tart-agent"].version) == "0.5.10"
    assert newest["kuiper-libvirt-agent"].published_at == ~U[2026-10-30 13:38:55Z]
  end

  test "check flags only agents behind the newest release" do
    assert AgentUpdates.check("proxmox", "0.1.0") == nil

    AgentUpdates.set(AgentUpdates.newest_agent_releases(@releases))
    assert %{latest: "0.5.0", url: "u-pve-050"} = AgentUpdates.check("proxmox", "0.4.3")
    assert %{latest: "0.5.0"} = AgentUpdates.check("proxmox", "v0.4.3")
    assert AgentUpdates.check("proxmox", "0.5.0") == nil
    # a dev build ahead of the newest release
    assert AgentUpdates.check("proxmox", "0.6.0") == nil
    assert AgentUpdates.check("tart", "0.5.9")
    assert AgentUpdates.check("proxmox", nil) == nil
    assert AgentUpdates.check("proxmox", "garbage") == nil
    assert AgentUpdates.check("unknown", "0.0.1") == nil
  end

  test "refresh follows pagination" do
    opts =
      stub_github([
        {200, release("kuiper-forge-v0.9.0")},
        {200, release("kuiper-tart-agent-v0.7.0")},
        {200, release("kuiper-proxmox-agent-v0.9.0")}
      ])

    assert :ok = AgentUpdates.refresh("http://github.test/releases?per_page=100", opts)
    assert %{latest: "0.9.0"} = AgentUpdates.check("proxmox", "0.5.0")
    assert %{latest: "0.7.0"} = AgentUpdates.check("tart", "0.5.0")
  end

  test "a failed page keeps the last good releases" do
    AgentUpdates.set(AgentUpdates.newest_agent_releases(@releases))

    opts = stub_github([{200, release("kuiper-proxmox-agent-v0.9.0")}, {500, "boom"}])
    assert {:error, msg} = AgentUpdates.refresh("http://github.test/releases", opts)
    assert msg =~ "page 2"
    # still the old answer, not page 1 on its own
    assert %{latest: "0.5.0"} = AgentUpdates.check("proxmox", "0.4.3")
    assert AgentUpdates.check("tart", "0.5.9")

    # a page that isn't a release list fails the same way
    opts =
      stub_github([{200, release("kuiper-proxmox-agent-v0.9.0")}, {200, %{"message" => "nope"}}])

    assert {:error, _} = AgentUpdates.refresh("http://github.test/releases", opts)
    assert %{latest: "0.5.0"} = AgentUpdates.check("proxmox", "0.4.3")
  end

  test "next_link" do
    header =
      ~s(<https://api.github.com/repositories/1/releases?per_page=100&page=2>; rel="next", <https://api.github.com/repositories/1/releases?per_page=100&page=4>; rel="last")

    assert AgentUpdates.next_link(header) ==
             "https://api.github.com/repositories/1/releases?per_page=100&page=2"

    assert AgentUpdates.next_link(
             ~s(<https://x/releases?page=1>; rel="prev", <https://x/releases?page=1>; rel="first")
           ) == nil

    assert AgentUpdates.next_link("") == nil
  end

  test "KUIPER_UPDATE_CHECK turns it off" do
    assert AgentUpdates.enabled_from_env(nil)
    assert AgentUpdates.enabled_from_env("true")
    for off <- ["0", "false", " No ", "off"], do: refute(AgentUpdates.enabled_from_env(off))
  end

  describe "child/0 (D49: no admin on/off switch)" do
    setup do
      old = Application.get_env(:kuiper_coordinator, :admin)
      on_exit(fn -> Application.put_env(:kuiper_coordinator, :admin, old || []) end)
    end

    test "runs on KUIPER_UPDATE_CHECK alone" do
      Application.put_env(:kuiper_coordinator, :admin, update_check: true)
      assert AgentUpdates.child() == AgentUpdates

      Application.put_env(:kuiper_coordinator, :admin, update_check: false)
      assert AgentUpdates.child() == nil
    end
  end
end
