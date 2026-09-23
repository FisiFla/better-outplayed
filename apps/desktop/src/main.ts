import { mount } from 'svelte';
import App from './App.svelte';
import './app.css';

const target = document.getElementById('app');
if (!target) {
  // Svelte 5 does not need this check, but failing here with a sentence beats a stack
  // trace from inside the runtime if `index.html` ever loses the mount point.
  throw new Error('localplay: #app is missing from index.html');
}

export default mount(App, { target });
