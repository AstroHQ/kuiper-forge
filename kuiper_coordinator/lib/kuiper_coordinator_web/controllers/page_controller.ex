defmodule KuiperCoordinatorWeb.PageController do
  use KuiperCoordinatorWeb, :controller

  def home(conn, _params) do
    render(conn, :home)
  end
end
