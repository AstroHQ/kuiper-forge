defmodule KuiperCoordinatorWeb.Admin.DashboardTokenRedactionTest do
  @moduledoc """
  The dashboard never puts a full registration token in its HTML (D65): the pending tokens table
  shows a short prefix, DOM ids and the delete button carry a non-secret handle, and the token
  itself is only inside the Register agent modal's bundle right after it's made (D60).
  """
  use KuiperCoordinatorWeb.ConnCase, async: false
  @moduletag :capture_log
  import Phoenix.LiveViewTest

  alias KuiperCoordinator.{Admin, Agents}

  setup %{conn: conn} do
    old = Application.get_env(:kuiper_coordinator, :admin)

    Application.put_env(:kuiper_coordinator, :admin,
      session_timeout_secs: 3600,
      coordinator_url: "https://ci.example:9543"
    )

    on_exit(fn -> Application.put_env(:kuiper_coordinator, :admin, old || []) end)
    Admin.LoginThrottle.reset()
    {:ok, _} = Admin.create_user("alice", "correct horse", "correct horse")
    conn = post(conn, ~p"/admin/login", %{"username" => "alice", "password" => "correct horse"})
    {ca_cert, ca_key} = KuiperCoordinator.AgentGrpcCase.fixture_ca()
    KuiperCoordinator.CA.install!(ca_cert, ca_key)
    %{conn: recycle(conn)}
  end

  defp secret(token), do: String.slice(token, 4, 32)

  test "pending tokens show a prefix only and delete by handle", %{conn: conn} do
    {:ok, t} = Agents.create_registration_token(3600, "cli")
    {:ok, other} = Agents.create_registration_token(3600, "cli")

    {:ok, view, html} = live(conn, ~p"/admin/dashboard")
    refute html =~ secret(t.token)
    refute render(view) =~ secret(t.token)
    refute render(view) =~ secret(other.token)

    for tok <- [t, other] do
      row = "#tokens #token-#{Agents.token_handle(tok.token)}"
      assert has_element?(view, row)
      assert has_element?(view, "#{row} code", String.slice(tok.token, 0, 8) <> "…")
      refute has_element?(view, "#{row} code", tok.token)
    end

    # register: the modal holds the new token's bundle, the page never holds the raw token
    html = view |> element("#register-agent") |> render_click()
    [new] = Enum.reject(Agents.list_registration_tokens(), &(&1.token in [t.token, other.token]))
    refute html =~ secret(new.token)
    html = view |> element("#register-close") |> render_click()
    refute html =~ secret(new.token)

    # delete still works, by a handle the server resolves, and only for that token
    view |> element("#token-#{Agents.token_handle(t.token)} button") |> render_click()
    remaining = Enum.map(Agents.list_registration_tokens(), & &1.token)
    refute t.token in remaining
    assert other.token in remaining

    # a raw token sent as the handle deletes nothing
    render_click(view, "delete_token", %{"handle" => other.token})
    assert other.token in Enum.map(Agents.list_registration_tokens(), & &1.token)
  end
end
