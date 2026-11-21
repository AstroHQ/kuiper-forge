defmodule KuiperCoordinatorWeb.Admin.ApiTokensLive do
  @moduledoc """
  Admin API tokens (parity UI9, api_token_routes.rs): create one (shown once, only in this
  LiveView's memory, never in the URL or flash cookie), list by prefix with last use, delete.
  """
  use KuiperCoordinatorWeb, :live_view

  alias KuiperCoordinator.Admin.ApiTokens

  @impl true
  def mount(_params, _session, socket) do
    {:ok, socket |> assign(page_title: "API tokens", new_token: nil) |> load()}
  end

  defp load(socket) do
    assign(socket, tokens: ApiTokens.list(), create_form: to_form(%{}, as: :token))
  end

  @impl true
  def handle_event("create", %{"token" => %{"name" => name}}, socket) do
    case ApiTokens.create(name, socket.assigns.current_admin.username) do
      {:ok, token, record} ->
        {:noreply,
         socket
         |> clear_flash()
         |> assign(new_token: %{name: record.name, token: token})
         |> load()}

      {:error, :invalid_name} ->
        {:noreply, put_flash(socket, :error, "Name must be 1-100 characters")}

      {:error, _} ->
        {:noreply, put_flash(socket, :error, "Failed to create token")}
    end
  end

  def handle_event("dismiss", _params, socket), do: {:noreply, assign(socket, new_token: nil)}

  def handle_event("delete", %{"id" => id}, socket) do
    name = Enum.find_value(socket.assigns.tokens, id, &(&1.id == id && &1.name))

    socket =
      case ApiTokens.delete(id) do
        :ok -> put_flash(socket, :info, "Deleted #{name}")
        {:error, :not_found} -> put_flash(socket, :error, "No such token")
      end

    {:noreply, socket |> assign(new_token: nil) |> load()}
  end

  defp when_(nil), do: "never"
  defp when_(dt), do: Calendar.strftime(dt, "%Y-%m-%d %H:%M UTC")

  @impl true
  def render(assigns) do
    ~H"""
    <Layouts.admin flash={@flash} current_admin={@current_admin} active={:api_tokens}>
      <.header>
        API tokens
        <:subtitle>
          Bearer tokens for the read-only JSON API at <code>/api/v1</code>
          (<code>status</code>, <code>agents</code>, <code>agents/:id/logs</code>).
        </:subtitle>
      </.header>

      <div
        :if={@new_token}
        id="new-token"
        role="alert"
        class="alert alert-success flex-col items-start"
      >
        <p>
          Token <strong>{@new_token.name}</strong> created. Copy it now: it won't be shown again.
        </p>
        <code class="select-all break-all font-mono">{@new_token.token}</code>
        <button class="btn btn-sm" phx-click="dismiss">Done</button>
      </div>

      <section class="card bg-base-200">
        <div class="card-body">
          <h2 class="card-title">New token</h2>
          <.form
            for={@create_form}
            id="create-token"
            phx-submit="create"
            class="flex flex-wrap gap-2 items-end"
          >
            <.input field={@create_form[:name]} label="Name" autocomplete="off" maxlength="100" />
            <.button>Create</.button>
          </.form>
        </div>
      </section>

      <p :if={@tokens == []} class="opacity-70">No API tokens yet.</p>

      <table :if={@tokens != []} class="table" id="api-tokens">
        <thead>
          <tr>
            <th>Name</th><th>Token</th><th>Created</th><th>Last used</th><th></th>
          </tr>
        </thead>
        <tbody>
          <tr :for={t <- @tokens} id={"token-#{t.id}"}>
            <td>{t.name}</td>
            <td><code>{t.token_prefix}…</code></td>
            <td>{when_(t.inserted_at)} by {t.created_by}</td>
            <td>{when_(t.last_used_at)}</td>
            <td>
              <button
                id={"delete-#{t.id}"}
                class="btn btn-error btn-xs"
                phx-click="delete"
                phx-value-id={t.id}
                data-confirm={"Delete #{t.name}? Anything using it stops working."}
              >
                Delete
              </button>
            </td>
          </tr>
        </tbody>
      </table>
    </Layouts.admin>
    """
  end
end
