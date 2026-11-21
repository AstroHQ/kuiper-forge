defmodule KuiperCoordinator.Forges.GitHubManifestTest do
  use ExUnit.Case, async: true

  alias KuiperCoordinator.Forges.GitHubManifest
  alias KuiperCoordinatorWeb.Endpoint

  @pem "-----BEGIN RSA PRIVATE KEY-----\nMIIfake\n-----END RSA PRIVATE KEY-----\n"

  describe "manifest/2" do
    test "points the webhook and redirect back at the coordinator, asks for runner permissions" do
      m = GitHubManifest.manifest("https://ci.example.com/")

      assert m["url"] == "https://ci.example.com"
      assert m["redirect_url"] == "https://ci.example.com/admin/forges/github/callback"

      assert m["hook_attributes"] == %{
               "url" => "https://ci.example.com/webhook",
               "active" => true
             }

      assert m["default_events"] == ["workflow_job"]
      assert m["public"] == false

      assert m["default_permissions"] == %{
               "administration" => "write",
               "organization_self_hosted_runners" => "write",
               "actions" => "read",
               "metadata" => "read"
             }

      # GitHub takes it as a JSON string in the form.
      assert {:ok, _} = m |> Jason.encode!() |> Jason.decode()
    end

    test "the hook URL uses the given webhook path" do
      m = GitHubManifest.manifest("https://ci.example.com/", webhook_path: "/hooks/github")
      assert m["hook_attributes"]["url"] == "https://ci.example.com/hooks/github"
    end

    test "webhook off leaves no events and an inactive hook" do
      m = GitHubManifest.manifest("https://ci.example.com", webhook: false, name: "Kuiper CI")
      assert m["name"] == "Kuiper CI"
      assert m["hook_attributes"]["active"] == false
      assert m["default_events"] == []
    end
  end

  describe "new_app_url/2" do
    test "the user's account or an org's, with the state" do
      assert GitHubManifest.new_app_url("s.t") ==
               "https://github.com/settings/apps/new?state=s.t"

      assert GitHubManifest.new_app_url("abc", org: "AstroHQ") ==
               "https://github.com/organizations/AstroHQ/settings/apps/new?state=abc"

      assert GitHubManifest.new_app_url("abc", org: "a/../b") =~
               "/organizations/a%2F..%2Fb/settings/apps/new"
    end
  end

  describe "state" do
    test "round trips, rejects tampering and expiry" do
      state = GitHubManifest.sign_state(Endpoint, %{admin: 7, org: "AstroHQ"})
      assert {:ok, %{admin: 7, org: "AstroHQ"}} = GitHubManifest.verify_state(Endpoint, state)
      assert {:error, :invalid_state} = GitHubManifest.verify_state(Endpoint, state <> "x")
      assert {:error, :invalid_state} = GitHubManifest.verify_state(Endpoint, nil)
      assert {:error, :invalid_state} = GitHubManifest.verify_state(Endpoint, "")

      assert {:error, :expired_state} =
               GitHubManifest.verify_state(Endpoint, state, max_age: -1)
    end
  end

  describe "convert/2 against a stubbed GitHub" do
    test "exchanges the code for the App's ID, key and webhook secret" do
      plug = fn conn ->
        assert conn.method == "POST"
        assert conn.request_path == "/app-manifests/abc123/conversions"
        assert Plug.Conn.get_req_header(conn, "authorization") == []

        conn
        |> Plug.Conn.put_resp_content_type("application/json")
        |> Plug.Conn.send_resp(
          201,
          Jason.encode!(%{
            "id" => 4242,
            "slug" => "kuiper-forge",
            "name" => "Kuiper Forge",
            "owner" => %{"login" => "AstroHQ"},
            "html_url" => "https://github.com/apps/kuiper-forge",
            "client_id" => "Iv1.abc",
            "client_secret" => "cs",
            "webhook_secret" => "whs",
            "pem" => @pem
          })
        )
      end

      assert {:ok, app} = GitHubManifest.convert("abc123", req_options: [plug: plug])
      assert app.app_id == 4242
      assert app.private_key == @pem
      assert app.webhook_secret == "whs"
      assert app.owner == "AstroHQ"

      assert GitHubManifest.install_url(app) ==
               "https://github.com/apps/kuiper-forge/installations/new"
    end

    test "a used or expired code is an error, not a crash" do
      plug = fn conn ->
        conn
        |> Plug.Conn.put_resp_content_type("application/json")
        |> Plug.Conn.send_resp(404, ~s({"message":"Not Found"}))
      end

      assert {:error, {:http, 404, "Not Found"}} =
               GitHubManifest.convert("gone", req_options: [plug: plug])
    end

    test "a 201 without an id or key is a bad response" do
      plug = fn conn ->
        conn
        |> Plug.Conn.put_resp_content_type("application/json")
        |> Plug.Conn.send_resp(201, ~s({"slug":"x"}))
      end

      assert {:error, :bad_response} = GitHubManifest.convert("c", req_options: [plug: plug])
    end

    test "codes that could change the path never reach GitHub" do
      plug = fn _conn -> flunk("should not be called") end

      for bad <- ["../user", "a/b", "a?b", "", nil] do
        assert {:error, :invalid_code} = GitHubManifest.convert(bad, req_options: [plug: plug])
      end
    end
  end
end
