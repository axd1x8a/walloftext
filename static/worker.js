import init, { worker_entry } from '/walloftext_frontend.js';

self.onmessage = async (e) => {
    if (e.data?.type === 'INIT_WASM') {
        await init({ module_or_path: e.data.module });
        worker_entry();
    }
};
