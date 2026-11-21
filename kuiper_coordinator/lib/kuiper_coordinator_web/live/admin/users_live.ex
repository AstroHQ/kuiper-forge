defmodule KuiperCoordinatorWeb.Admin.UsersLive do
  @moduledoc """
  Admin users (parity UI8, user_routes.rs): list, create (1-64 of `[A-Za-z0-9._-@]`, with a
  confirm field), set a password (ends that user's sessions, keeps your own), delete (not
  yourself; never the last admin, which `Admin.delete_user/2` checks under a row lock).
  """
  use KuiperCoordinatorWeb, :live_view

  alias KuiperCoordinator.Admin

  @impl true
  def mount(_params, _session, socket) do
    {:ok, socket |> assign(page_title: "Users") |> load()}
  end

  defp load(socket) do
    assign(socket, users: Admin.list_users(), create_form: to_form(%{}, as: :user))
  end

  @impl true
  def handle_event("create", %{"user" => p}, socket) do
    case Admin.create_user(p["username"], p["password"], p["confirm"]) do
      {:ok, user} ->
        {:noreply, socket |> put_flash(:info, "Created #{user.username}") |> load()}

      {:error, reason} ->
        {:noreply,
         socket
         |> put_flash(:error, message(reason))
         |> assign(create_form: to_form(Map.take(p, ["username"]), as: :user))}
    end
  end

  def handle_event("set_password", %{"username" => name, "pw" => p}, socket) do
    keep = if name == socket.assigns.current_admin.username, do: socket.assigns.admin_session_id

    case Admin.set_password(name, p["password"], p["confirm"], keep: keep) do
      :ok -> {:noreply, socket |> put_flash(:info, "Password set for #{name}") |> load()}
      {:error, reason} -> {:noreply, put_flash(socket, :error, message(reason))}
    end
  end

  def handle_event("delete", %{"username" => name}, socket) do
    case Admin.delete_user(name, socket.assigns.current_admin.username) do
      :ok -> {:noreply, socket |> put_flash(:info, "Deleted #{name}") |> load()}
      {:error, reason} -> {:noreply, socket |> put_flash(:error, message(reason)) |> load()}
    end
  end

  defp message(:invalid_username),
    do: "Usernames are 1-64 characters: letters, digits, and . _ - @"

  defp message(:empty_password), do: "Password can't be empty"
  defp message(:password_mismatch), do: "Passwords don't match"
  defp message(:username_taken), do: "That username already exists"
  defp message(:self), do: "You can't delete yourself"
  defp message(:last_admin), do: "Can't delete the last admin"
  defp message(:not_found), do: "No such user"
  defp message(_), do: "Something went wrong"

  @impl true
  def render(assigns) do
    ~H"""
    <Layouts.admin flash={@flash} current_admin={@current_admin} active={:users}>
      <.header>Users</.header>

      <section class="card bg-base-200">
        <div class="card-body">
          <h2 class="card-title">Add an admin</h2>
          <.form
            for={@create_form}
            id="create-user"
            phx-submit="create"
            class="flex flex-wrap gap-2 items-end"
          >
            <.input field={@create_form[:username]} label="Username" autocomplete="off" />
            <.input
              field={@create_form[:password]}
              type="password"
              label="Password"
              autocomplete="new-password"
            />
            <.input
              field={@create_form[:confirm]}
              type="password"
              label="Confirm"
              autocomplete="new-password"
            />
            <.button>Create</.button>
          </.form>
        </div>
      </section>

      <table class="table" id="users">
        <thead>
          <tr>
            <th>Username</th><th>Created</th><th>New password</th><th></th>
          </tr>
        </thead>
        <tbody>
          <tr :for={u <- @users} id={"user-#{u.username}"}>
            <td>
              {u.username}
              <span :if={u.username == @current_admin.username} class="badge badge-ghost badge-sm">you</span>
            </td>
            <td>{Calendar.strftime(u.inserted_at, "%Y-%m-%d %H:%M UTC")}</td>
            <td>
              <form id={"password-#{u.username}"} phx-submit="set_password" class="flex gap-2">
                <input type="hidden" name="username" value={u.username} />
                <input
                  type="password"
                  name="pw[password]"
                  placeholder="Password"
                  class="input input-sm"
                  autocomplete="new-password"
                />
                <input
                  type="password"
                  name="pw[confirm]"
                  placeholder="Confirm"
                  class="input input-sm"
                  autocomplete="new-password"
                />
                <button type="submit" class="btn btn-sm">Set</button>
              </form>
            </td>
            <td>
              <button
                :if={u.username != @current_admin.username}
                id={"delete-#{u.username}"}
                class="btn btn-error btn-xs"
                phx-click="delete"
                phx-value-username={u.username}
                data-confirm={"Delete #{u.username}?"}
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
