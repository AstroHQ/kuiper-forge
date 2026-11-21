defmodule KuiperCoordinatorWeb.Admin.DashboardLive do
  @moduledoc "Admin dashboard (parity UI2)."
  use KuiperCoordinatorWeb, :live_view

  @impl true
  def mount(_params, _session, socket) do
    {:ok, assign(socket, page_title: "Dashboard")}
  end

  @impl true
  def render(assigns) do
    ~H"""
    <Layouts.admin flash={@flash} current_admin={@current_admin} active={:dashboard}>
      <.header>Dashboard</.header>
    </Layouts.admin>
    """
  end
end
