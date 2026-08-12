import { mount } from "svelte";
import App from "./App.svelte";
import "./styles/tokens.css";
import "./styles/base.css";

const target = document.getElementById("app");
if (target === null) throw new Error("missing #app mount point in index.html");
mount(App, { target });
