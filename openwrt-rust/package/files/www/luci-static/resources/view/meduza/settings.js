'use strict';
'require view';
'require form';
'require uci';
'require ui';
'require rpc';
'require fs';
'require poll';
'require dom';

var callLogRead = rpc.declare({
	object: 'log',
	method: 'read',
	params: [ 'lines', 'stream', 'oneshot' ],
	expect: { log: [] }
});

function validateNodeId(sectionId, value) {
	if (value.length === 0)
		return _('Node ID is required.');
	if (value.length > 128 || !/^[A-Za-z0-9_][A-Za-z0-9_.-]*$/.test(value))
		return _('Use at most 128 ASCII letters, digits, dots, dashes or underscores; the first character cannot be a dot or dash.');
	return true;
}

function validateEndpoints(sectionId, value) {
	var entries = value.split(',').map(function(entry) { return entry.trim(); })
		.filter(function(entry) { return entry.length > 0; });
	if (entries.length === 0)
		return _('At least one etcd endpoint is required.');
	for (var i = 0; i < entries.length; i++) {
		var candidate = entries[i].indexOf('://') >= 0 ? entries[i] : 'https://' + entries[i];
		if (!/^https?:\/\/(?:\[[^\]]+\]|[^\/:?#]+):[0-9]+\/?$/i.test(candidate))
			return _('Each endpoint must be an HTTP(S) host and explicit port without credentials, a path, query or fragment.');
		try {
			var parsed = new URL(candidate);
			var rawPort = candidate.match(/:([0-9]+)\/?$/);
			var port = rawPort ? Number(rawPort[1]) : 0;
			if ((parsed.protocol !== 'http:' && parsed.protocol !== 'https:') ||
			    !parsed.hostname || parsed.username || parsed.password ||
			    parsed.pathname !== '/' || parsed.search || parsed.hash ||
			    port < 1 || port > 65535)
				return _('An etcd endpoint is invalid.');
		}
		catch (error) {
			return _('An etcd endpoint is invalid.');
		}
	}
	return true;
}

function pairedValue(peer, message) {
	return function(sectionId, value) {
		var options = this.map.lookupOption(peer, sectionId);
		var other = options.length > 0 ? options[0].formvalue(sectionId) : '';
		return (String(value || '').trim().length > 0) ===
			(String(other || '').trim().length > 0) ? true : message;
	};
}

function statusClass(state) {
	return state === 'up' || state === 'connected' ? 'success' :
		state === 'connecting' || state === 'waiting' ? 'warning' : 'danger';
}

function statusBadge(state) {
	return E('span', {
		'class': 'label ' + statusClass(state),
		'style': 'display:inline-block;min-width:6em;text-align:center'
	}, [ state || _('unknown') ]);
}

function emptyRow(columns, message) {
	return E('tr', {}, [ E('td', { 'colspan': columns }, [ E('em', {}, [ message ]) ]) ]);
}

function fetchStatus() {
	return L.resolveDefault(
		fs.exec_direct('/usr/sbin/meduza-openwrt', [ 'status', '--json' ], 'json'),
		{ etcd: { state: 'unknown' }, interface_details: [], frr: 'down' }
	);
}

function fetchMeduzaLog() {
	return L.resolveDefault(callLogRead(500, false, true), []).then(function(entries) {
		return entries.filter(function(entry) {
			var line = [ entry.msg, entry.source, entry.ident, entry.process ]
				.map(function(value) { return String(value || ''); }).join(' ').toLowerCase();
			return line.indexOf('meduza') >= 0;
		}).slice(-300);
	});
}

function renderLog(entries) {
	return entries.map(function(entry) {
		var numeric = Number(entry.time);
		var date = isNaN(numeric) ? new Date(entry.time) :
			new Date(numeric < 1000000000000 ? numeric * 1000 : numeric);
		var time = entry.time && !isNaN(date.getTime()) ? date.toLocaleString() : '';
		return '[' + time + '] ' + String(entry.msg || '');
	}).join('\n');
}

return view.extend({
	load: function() {
		return Promise.all([
			uci.load('meduza'),
			L.resolveDefault(uci.load('firewall'), null),
			fetchStatus(),
			fetchMeduzaLog()
		]);
	},

	updateStatus: function(status) {
		var etcd = status.etcd || { state: 'unknown' };
		var etcdBox = document.getElementById('meduza-etcd-status');
		if (etcdBox)
			dom.content(etcdBox, [
				statusBadge(etcd.state), ' ', E('strong', {}, [ etcd.node_id || '-' ]), E('br'),
				E('small', {}, [ _('Commit: '), etcd.commit || '-', ' · ', etcd.updated_at || '-' ])
			]);

		var details = Array.isArray(status.interface_details) ? status.interface_details : [];
		var outbound = details.filter(function(item) { return item.kind !== 'tinc'; });
		var tinc = details.filter(function(item) { return item.kind === 'tinc'; });
		var outboundBody = document.getElementById('meduza-outbound-body');
		if (outboundBody)
			dom.content(outboundBody, outbound.length ? outbound.map(function(item) {
				return E('tr', {}, [
					E('td', {}, [ item.kind ]), E('td', {}, [ item.instance ]),
					E('td', {}, [ item.device ]), E('td', {}, [ statusBadge(item.state) ])
				]);
			}) : [ emptyRow(4, _('No outbound VPN connections are managed.')) ]);

		var tincBody = document.getElementById('meduza-tinc-body');
		if (tincBody)
			dom.content(tincBody, tinc.length ? tinc.map(function(item) {
				return E('tr', {}, [
					E('td', {}, [ item.instance ]), E('td', {}, [ item.device ]),
					E('td', {}, [ statusBadge(item.state) ])
				]);
			}) : [ emptyRow(3, _('Tinc is not managed on this node.')) ]);

		var frr = document.getElementById('meduza-frr-status');
		if (frr)
			dom.content(frr, statusBadge(status.frr || 'down'));
	},

	updateLog: function(entries) {
		var log = document.getElementById('meduza-log');
		if (log) {
			log.value = renderLog(entries);
			log.scrollTop = log.scrollHeight;
		}
	},

	render: function(data) {
		var m = new form.Map('meduza', _('Settings'),
			_('Only controller settings are stored in UCI. VPN interfaces and FRR are created and owned directly by meduza-openwrt.'));
		var s = m.section(form.NamedSection, 'main', 'meduza', _('Controller settings'));
		var o;
		s.addremove = false;

		o = s.option(form.Flag, 'enable', _('Enable controller'));
		o.default = o.disabled;
		o.rmempty = false;

		o = s.option(form.ListValue, 'VPN_FIREWALL_ZONE', _('VPN firewall zone'));
		o.rmempty = true;
		o.value('', _('Do not manage firewall membership'));
		var knownZones = {};
		uci.sections('firewall', 'zone', function(zone) {
			var name = String(zone.name || '');
			if (/^[A-Za-z0-9_][A-Za-z0-9_.-]{0,63}$/.test(name) && !knownZones[name]) {
				knownZones[name] = true;
				o.value(name, name);
			}
		});
		var configuredZone = String(uci.get('meduza', 'main', 'VPN_FIREWALL_ZONE') || '');
		if (configuredZone && !knownZones[configuredZone])
			o.value(configuredZone, configuredZone + ' (' + _('missing') + ')');
		o.description = _('All Meduza-managed Tinc, OpenVPN and WireGuard device names are added to this zone. Existing members and all zone policies remain administrator-owned.');

		o = s.option(form.Value, 'NODE_ID', _('Node ID'));
		o.rmempty = false;
		o.validate = validateNodeId;
		o.placeholder = 'router-01';

		o = s.option(form.Value, 'ETCD_ENDPOINTS', _('etcd endpoints'));
		o.rmempty = false;
		o.validate = validateEndpoints;
		o.placeholder = 'https://etcd.example.net:2379';

		o = s.option(form.Value, 'ETCD_CA', _('CA certificate'));
		o.rmempty = true;
		o.placeholder = '/etc/meduza/pki/ca.crt';

		o = s.option(form.Value, 'ETCD_CERT', _('Client certificate'));
		o.rmempty = true;
		o.validate = pairedValue('ETCD_KEY', _('Client certificate and client key must be configured together.'));
		o.placeholder = '/etc/meduza/pki/client.crt';

		o = s.option(form.Value, 'ETCD_KEY', _('Client private key'));
		o.rmempty = true;
		o.validate = pairedValue('ETCD_CERT', _('Client certificate and client key must be configured together.'));
		o.placeholder = '/etc/meduza/pki/client.key';

		o = s.option(form.Value, 'ETCD_USER', _('etcd username'));
		o.rmempty = true;
		o.validate = pairedValue('ETCD_PASS', _('etcd username and password must be configured together.'));

		o = s.option(form.Value, 'ETCD_PASS', _('etcd password'));
		o.password = true;
		o.rmempty = true;
		o.validate = pairedValue('ETCD_USER', _('etcd username and password must be configured together.'));

		return m.render().then(L.bind(function(settingsForm) {
			var outboundTable = E('table', { 'class': 'table cbi-section-table' }, [
				E('tr', { 'class': 'tr table-titles' }, [
					E('th', {}, [ _('Type') ]), E('th', {}, [ _('Instance') ]),
					E('th', {}, [ _('Interface') ]), E('th', {}, [ _('Status') ])
				]), E('tbody', { 'id': 'meduza-outbound-body' })
			]);
			var tincTable = E('table', { 'class': 'table cbi-section-table' }, [
				E('tr', { 'class': 'tr table-titles' }, [
					E('th', {}, [ _('Instance') ]), E('th', {}, [ _('Interface') ]),
					E('th', {}, [ _('Status') ])
				]), E('tbody', { 'id': 'meduza-tinc-body' })
			]);
			var tabs = E('div', {}, [
				E('div', { 'class': 'cbi-section', 'data-tab': 'status', 'data-tab-title': _('Status') }, [
					E('h3', {}, [ _('etcd status') ]),
					E('div', { 'id': 'meduza-etcd-status', 'class': 'cbi-value-description' }),
					E('h3', {}, [ _('Managed outbound VPN connections') ]), outboundTable,
					E('h3', {}, [ _('Meduza logs') ]),
					E('textarea', { 'id': 'meduza-log', 'readonly': 'readonly', 'wrap': 'off',
						'rows': 18, 'style': 'width:100%;font:12px monospace' })
				]),
				E('div', { 'class': 'cbi-section', 'data-tab': 'routing', 'data-tab-title': _('Tinc & FRR') }, [
					E('h3', {}, [ _('Tinc status') ]), tincTable,
					E('h3', {}, [ _('FRR status') ]), E('div', { 'id': 'meduza-frr-status' })
				]),
				E('div', { 'class': 'cbi-section', 'data-tab': 'settings', 'data-tab-title': _('Settings') }, [ settingsForm ])
			]);
			var page = E([], [ E('h2', {}, [ _('Meduza') ]), tabs ]);
			ui.tabs.initTabGroup(tabs.childNodes);
			window.setTimeout(L.bind(function() {
				this.updateStatus(data[2]);
				this.updateLog(data[3]);
			}, this), 0);
			poll.add(L.bind(function() {
				return Promise.all([ fetchStatus(), fetchMeduzaLog() ]).then(L.bind(function(values) {
					this.updateStatus(values[0]);
					this.updateLog(values[1]);
				}, this));
			}, this), 5);
			return page;
		}, this));
	}
});
