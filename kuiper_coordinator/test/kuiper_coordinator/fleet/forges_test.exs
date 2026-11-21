defmodule KuiperCoordinator.Fleet.ForgesTest do
  # Nye's Forgejo read (Nov 21), item 1: forges are built from config and validated at
  # startup (Rust main.rs, parity C2, C3, C10, G1, FJ7).
  use KuiperCoordinator.DataCase, async: false

  alias KuiperCoordinator.Fleet.{Forges, Manager}
  alias KuiperCoordinator.Forgejo.Client, as: Forgejo
  alias KuiperCoordinator.GitHub.Client, as: GitHub

  # A Forgejo that lets the token do everything, or (read_only) only read.
  defp forgejo(read_only \\ false) do
    name = {__MODULE__, make_ref()}

    Req.Test.stub(name, fn conn ->
      case {conn.method, conn.path_info} do
        {"GET", ["api", "v1", "repositories", "0"]} ->
          conn |> Plug.Conn.put_status(404) |> Req.Test.json(%{"message" => "not found"})

        {"GET", ["api", "v1", "orgs", "acme", "actions", "runners", "jobs"]} ->
          Req.Test.json(conn, [])

        {"DELETE", ["api", "v1", "orgs", "acme", "actions", "runners", "0"]} when read_only ->
          conn
          |> Plug.Conn.put_status(403)
          |> Req.Test.json(%{
            "message" =>
              "token does not have at least one of required scope(s): [write:organization]"
          })

        {"DELETE", _} ->
          conn |> Plug.Conn.put_status(404) |> Req.Test.json(%{"message" => "not found"})
      end
    end)

    [plug: {Req.Test, name}, retry: false]
  end

  defp github(installations) do
    name = {__MODULE__, make_ref()}

    Req.Test.stub(name, fn conn ->
      case conn.request_path do
        "/app/installations" -> Req.Test.json(conn, installations)
      end
    end)

    [plug: {Req.Test, name}, retry: false]
  end

  defp pem do
    key = :public_key.generate_key({:rsa, 2048, 65_537})
    :public_key.pem_encode([:public_key.pem_entry_encode(:RSAPrivateKey, key)])
  end

  @fj %{name: "fj", type: :forgejo, url: "http://forgejo.test", token: "t", scopes: ["acme"]}

  test "a Forgejo spec becomes a validated client with its scopes" do
    assert {:ok, [{"fj", {Forgejo, %Forgejo{scopes: [{:org, "acme"}]}}}]} =
             Forges.build([@fj], req_options: forgejo())
  end

  test "a read-only Forgejo token fails startup with the write-access hint (FJ7)" do
    assert {:error, msg} = Forges.build([@fj], req_options: forgejo(true))
    assert msg =~ "forge 'fj'"
    assert msg =~ "can't manage runners"
    assert msg =~ "write:organization"
  end

  test "a GitHub spec is validated: the App needs an installation (G1)" do
    gh = [name: "github", type: :github, app_id: "42", private_key: pem()]

    assert {:ok, [{"github", {GitHub, %GitHub{app_id: 42}}}]} =
             Forges.build([gh],
               req_options: github([%{"id" => 7, "account" => %{"login" => "astro"}}])
             )

    assert {:error, "forge 'github': the GitHub App has no installations"} =
             Forges.build([gh], req_options: github([]))

    assert {:error, msg} = Forges.build([[name: "github", type: :github, app_id: 42]])
    assert msg =~ "private key"
  end

  test "names are unique and colon-free, types known, at least one forge (C2)" do
    assert {:error, msg} = Forges.build([@fj, @fj], validate: false)
    assert msg =~ "unique"
    assert {:error, msg} = Forges.build([%{@fj | name: "a:b"}], validate: false)
    assert msg =~ "':'"
    assert {:error, msg} = Forges.build([%{@fj | type: :gitlab}], validate: false)
    assert msg =~ "unknown forge type"
    assert {:error, _} = Forges.build([])
  end

  test "the webhook's source: named, else the first GitHub, else the first (W1); Forgejo brings its scopes" do
    {:ok, built} =
      Forges.build(
        [@fj, [name: "github", type: :github, app_id: 1, private_key: "x"]],
        validate: false
      )

    assert {:ok, w} = Forges.webhook_config(built, secret: "s")
    assert w[:forge] == "github" and w[:source] == :github and w[:secret] == "s"

    assert {:ok, w} = Forges.webhook_config(built, forge: "fj")
    assert w[:source] == :forgejo and w[:scopes] == [{:org, "acme"}]

    assert {:ok, w} = Forges.webhook_config(Enum.take(built, 1), [])
    assert w[:forge] == "fj"

    assert {:error, _} = Forges.webhook_config(built, forge: "nope")
  end

  describe "the fleet manager builds its forges at startup" do
    setup do
      prev = Application.get_env(:kuiper_coordinator, :webhook)

      on_exit(fn ->
        if prev,
          do: Application.put_env(:kuiper_coordinator, :webhook, prev),
          else: Application.delete_env(:kuiper_coordinator, :webhook)
      end)

      Application.put_env(:kuiper_coordinator, :webhook, secret: "s", forge: "fj")
      :ok
    end

    defp init(mode, req_options),
      do:
        Manager.init(
          name: nil,
          mode: mode,
          forges: [@fj],
          req_options: req_options,
          tick_ms: 60_000
        )

    test "a forge that fails validation stops startup" do
      assert {:stop, {:forges, msg}} = init(:webhook, forgejo(true))
      assert msg =~ "write:organization"
    end

    test "webhook mode points the webhook at the Forgejo forge and its scopes" do
      assert {:ok, %{ctx: %{forge_name: "fj", forges: %{"fj" => {Forgejo, _}}}}, _} =
               init(:webhook, forgejo())

      w = Application.get_env(:kuiper_coordinator, :webhook)
      assert w[:source] == :forgejo and w[:scopes] == [{:org, "acme"}] and w[:secret] == "s"
    end

    test "fixed capacity with a Forgejo forge is refused (C3)" do
      assert {:stop, {:forges, msg}} = init(:fixed_capacity, forgejo())
      assert msg =~ "fixed_capacity"
    end
  end
end
