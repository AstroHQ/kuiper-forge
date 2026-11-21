defmodule KuiperCoordinator.ForgesTest do
  # D39: forges and provisioning settings in Postgres, secrets encrypted, checked at save.
  use KuiperCoordinator.DataCase, async: false

  alias KuiperCoordinator.Forges
  alias KuiperCoordinator.Forges.Forge

  defp pem do
    key = :public_key.generate_key({:rsa, 2048, 65_537})
    :public_key.pem_encode([:public_key.pem_entry_encode(:RSAPrivateKey, key)])
  end

  # a Forgejo whose token reads the scope and, unless read_only, may manage its runners
  defp forgejo(read_only) do
    name = {__MODULE__, make_ref()}

    Req.Test.stub(name, fn conn ->
      case {conn.method, conn.path_info} do
        {"GET", ["api", "v1", "repositories", "0"]} ->
          conn |> Plug.Conn.put_status(404) |> Req.Test.json(%{"message" => "not found"})

        {"GET", ["api", "v1", "orgs", "acme", "actions", "runners", "jobs"]} ->
          Req.Test.json(conn, [])

        {"DELETE", _} when read_only ->
          conn
          |> Plug.Conn.put_status(403)
          |> Req.Test.json(%{
            "message" => "token does not have at least one of required scope(s)"
          })

        {"DELETE", _} ->
          conn |> Plug.Conn.put_status(404) |> Req.Test.json(%{"message" => "not found"})
      end
    end)

    [plug: {Req.Test, name}, retry: false]
  end

  defp github_attrs(extra \\ %{}),
    do: Map.merge(%{name: "github", type: :github, app_id: 42, private_key: pem()}, extra)

  defp forgejo_attrs(extra \\ %{}),
    do:
      Map.merge(
        %{
          name: "forgejo",
          type: :forgejo,
          url: "https://git.example.com/",
          token: " tok \n",
          scopes: ["acme"]
        },
        extra
      )

  defp errors(cs), do: errors_on(cs)

  describe "forges" do
    test "secrets are encrypted at rest and read back decrypted" do
      key = pem()

      assert {:ok, forge} =
               Forges.create_forge(github_attrs(%{private_key: key, webhook_secret: "s3cret"}))

      %{rows: [[raw_key, raw_secret]]} =
        Repo.query!("SELECT private_key, webhook_secret FROM forges WHERE id = $1", [forge.id])

      refute raw_key =~ "PRIVATE KEY"
      refute raw_secret =~ "s3cret"
      assert %Forge{private_key: ^key, webhook_secret: "s3cret"} = Forges.get_forge!(forge.id)
      refute inspect(forge) =~ "PRIVATE KEY"
    end

    test "Forgejo fields are tidied: trailing slash, token whitespace" do
      assert {:ok, f} = Forges.create_forge(forgejo_attrs())
      assert f.url == "https://git.example.com"
      assert Forges.get_forge!(f.id).token == "tok"
    end

    test "an inline PEM with literal \\n is unescaped (F12)" do
      escaped = String.replace(pem(), "\n", "\\n")
      assert {:ok, f} = Forges.create_forge(github_attrs(%{private_key: escaped}))
      assert Forges.get_forge!(f.id).private_key =~ "-----\nMII"
    end

    test "a blank secret on update keeps the stored one" do
      {:ok, f} = Forges.create_forge(forgejo_attrs(%{webhook_secret: "hook"}))

      assert {:ok, f} =
               Forges.update_forge(f, %{
                 "token" => "",
                 "webhook_secret" => "",
                 "scopes" => ["acme", "o/r"]
               })

      f = Forges.get_forge!(f.id)
      assert {f.token, f.webhook_secret, f.scopes} == {"tok", "hook", ["acme", "o/r"]}
    end

    test "bad fields are rejected at save, nothing stored (C2, C10, FJ7)" do
      assert {:error, cs} = Forges.create_forge(github_attrs(%{name: "a:b"}))
      assert errors(cs)[:name]
      assert {:error, cs} = Forges.create_forge(github_attrs(%{app_id: 0}))
      assert errors(cs)[:app_id]
      assert {:error, cs} = Forges.create_forge(github_attrs(%{private_key: "nope"}))
      assert errors(cs)[:private_key]
      assert {:error, cs} = Forges.create_forge(%{name: "x", type: :github})
      assert errors(cs)[:app_id] && errors(cs)[:private_key]
      assert {:error, cs} = Forges.create_forge(forgejo_attrs(%{url: "git.example.com"}))
      assert errors(cs)[:url]
      assert {:error, cs} = Forges.create_forge(forgejo_attrs(%{scopes: []}))
      assert errors(cs)[:scopes]
      assert {:error, cs} = Forges.create_forge(%{name: "x", type: :gitlab})
      assert errors(cs)[:type]
      assert Forges.list_forges() == []
    end

    test "Forgejo scopes follow Rust's runner_scopes rules (F13)" do
      for bad <- ["@", "@me/repo", "*/repo", "/repo"] do
        assert {:error, cs} = Forges.create_forge(forgejo_attrs(%{scopes: [bad]})), bad
        assert errors(cs)[:scopes], bad
      end

      for good <- ["acme", "owner/repo", "@user", "*"] do
        assert {:ok, f} =
                 Forges.create_forge(forgejo_attrs(%{name: "fj-#{good}", scopes: [good]}))

        Forges.delete_forge(f)
      end
    end

    test "names are unique and the type can't change" do
      {:ok, f} = Forges.create_forge(github_attrs())
      assert {:error, cs} = Forges.create_forge(github_attrs())
      assert errors(cs)[:name]
      assert {:error, cs} = Forges.update_forge(f, %{type: :forgejo})
      assert errors(cs)[:type]
    end

    test "a forge that fails its live check is rejected at save, not at boot (D32)" do
      assert {:error, cs} =
               Forges.create_forge(forgejo_attrs(), validate: true, req_options: forgejo(true))

      assert errors(cs)[:base] |> hd() =~ "forge 'forgejo'"
      assert Forges.list_forges() == []

      assert {:ok, _} =
               Forges.create_forge(forgejo_attrs(), validate: true, req_options: forgejo(false))
    end

    test "list puts GitHub forges first, then in the order added" do
      {:ok, _} = Forges.create_forge(forgejo_attrs())
      {:ok, _} = Forges.create_forge(github_attrs())
      assert Enum.map(Forges.list_forges(), & &1.name) == ["github", "forgejo"]
    end

    test "every change is broadcast" do
      Forges.subscribe()
      {:ok, f} = Forges.create_forge(forgejo_attrs())
      assert_receive {:forges_changed}
      {:ok, f} = Forges.update_forge(f, %{scopes: ["*"]})
      assert_receive {:forges_changed}
      {:ok, _} = Forges.delete_forge(f)
      assert_receive {:forges_changed}
      {:ok, _} = Forges.update_settings(%{mode: :polling})
      assert_receive {:forges_changed}
      {:ok, m} = Forges.create_label_mapping(%{labels: ["gpu"]})
      assert_receive {:forges_changed}
      {:ok, _} = Forges.delete_label_mapping(m)
      assert_receive {:forges_changed}
      assert {:error, _} = Forges.create_forge(%{name: "bad"})
      refute_receive {:forges_changed}, 50
    end
  end

  describe "settings" do
    test "defaults until saved, then stored as one row" do
      assert %{mode: :fixed_capacity, idle_runner_timeout_s: 600, poll_interval_s: 120} =
               Forges.get_settings()

      assert {:ok, _} = Forges.update_settings(%{mode: :webhook, runner_scope: "acme"})
      assert {:ok, _} = Forges.update_settings(%{poll_interval_s: 30})
      assert %{mode: :webhook, runner_scope: "acme", poll_interval_s: 30} = Forges.get_settings()
      assert %{rows: [[1]]} = Repo.query!("SELECT count(*) FROM provisioning_settings")
    end

    test "bad values are rejected; a short idle timeout is raised to 120 s, 0 is off" do
      assert {:error, cs} = Forges.update_settings(%{mode: :sometimes})
      assert errors(cs)[:mode]
      assert {:error, cs} = Forges.update_settings(%{runner_scope: "*/repo"})
      assert errors(cs)[:runner_scope]
      assert {:error, cs} = Forges.update_settings(%{poll_interval_s: 5})
      assert errors(cs)[:poll_interval_s]

      assert {:ok, %{idle_runner_timeout_s: 120}} =
               Forges.update_settings(%{idle_runner_timeout_s: 60})

      assert {:ok, %{idle_runner_timeout_s: 0}} =
               Forges.update_settings(%{idle_runner_timeout_s: 0})
    end

    test "label mappings need labels and a valid scope" do
      assert {:error, cs} = Forges.create_label_mapping(%{labels: []})
      assert errors(cs)[:labels]
      assert {:error, cs} = Forges.create_label_mapping(%{labels: ["gpu"], runner_scope: "@"})
      assert errors(cs)[:runner_scope]
    end
  end

  describe "fleet_config/0" do
    test "no forges: no fleet" do
      assert {:ok, %{fleet: nil, webhook: []}} = Forges.fleet_config()
    end

    test "fixed capacity: the GitHub forge and a parsed runner scope" do
      {:ok, _} = Forges.create_forge(github_attrs())
      assert {:error, msg} = Forges.fleet_config()
      assert msg =~ "runner scope"

      {:ok, _} = Forges.update_settings(%{runner_scope: "acme/repo", runner_group: "g"})
      assert {:ok, %{fleet: fleet, webhook: []}} = Forges.fleet_config()
      assert fleet[:mode] == :fixed_capacity
      assert fleet[:runner_scope] == {:repo, "acme", "repo"}
      assert fleet[:runner_group] == "g"

      assert [%{name: "github", type: :github, app_id: 42, scan_lookback_hours: 168}] =
               fleet[:forges]
    end

    test "fixed capacity with only Forgejo is refused (C3)" do
      {:ok, _} = Forges.create_forge(forgejo_attrs())
      {:ok, _} = Forges.update_settings(%{runner_scope: "acme"})
      assert {:error, msg} = Forges.fleet_config()
      assert msg =~ "GitHub"
    end

    test "polling: the interval, required labels and label mappings" do
      {:ok, _} = Forges.create_forge(forgejo_attrs())

      {:ok, _} =
        Forges.update_settings(%{
          mode: :polling,
          poll_interval_s: 30,
          required_labels: ["self-hosted", " linux "]
        })

      {:ok, _} =
        Forges.create_label_mapping(%{labels: ["gpu"], runner_scope: "acme", forge: "forgejo"})

      assert {:ok, %{fleet: fleet, webhook: webhook}} = Forges.fleet_config()
      assert fleet[:mode] == :polling
      assert fleet[:poll_interval_s] == 30

      assert [%{name: "forgejo", url: "https://git.example.com", token: "tok", scopes: ["acme"]}] =
               fleet[:forges]

      assert webhook[:required_labels] == ["self-hosted", "linux"]

      assert webhook[:label_mappings] == [
               %{
                 labels: ["gpu"],
                 runner_scope: {:org, "acme"},
                 runner_group: nil,
                 forge: "forgejo"
               }
             ]
    end

    test "webhook: the secret is the source forge's, kept as text (F14)" do
      {:ok, _} = Forges.create_forge(forgejo_attrs())
      {:ok, _} = Forges.update_settings(%{mode: :webhook})
      assert {:error, msg} = Forges.fleet_config()
      assert msg =~ "webhook secret"

      {:ok, _} = Forges.create_forge(github_attrs(%{webhook_secret: "0123"}))
      assert {:ok, %{webhook: webhook}} = Forges.fleet_config()
      assert webhook[:secret] == "0123"
      assert webhook[:forge] == "github"
      assert webhook[:path] == "/webhook"

      {:ok, _} = Forges.update_settings(%{webhook_forge: "nope"})
      assert {:error, msg} = Forges.fleet_config()
      assert msg =~ "nope"
    end
  end
end
