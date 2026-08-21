'use strict';
'require view';
'require form';
'require uci';

function validateNodeId(sectionId, value) {
	if (value.length === 0)
		return _('Node ID is required.');

	if (value.length > 128 || !/^[A-Za-z0-9_][A-Za-z0-9_.-]*$/.test(value))
		return _('Use at most 128 ASCII letters, digits, dots, dashes or underscores; the first character cannot be a dot or dash.');

	return true;
}

function validateEndpoints(sectionId, value) {
	var entries = value.split(',').map(function(entry) {
		return entry.trim();
	}).filter(function(entry) {
		return entry.length > 0;
	});

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
		var present = String(value || '').trim().length > 0;
		var otherPresent = String(other || '').trim().length > 0;

		return present === otherPresent ? true : message;
	};
}

return view.extend({
	load: function() {
		return Promise.all([
			uci.load('meduza'),
			uci.load('firewall').then(function() {
				return true;
			}, function() {
				return false;
			})
		]);
	},

	render: function(data) {
		var firewallAvailable = data[1];
		var zones = {};

		if (firewallAvailable) {
			uci.sections('firewall', 'zone', function(section) {
				if (section.name)
					zones[section.name] = true;
			});
		}

		var m = new form.Map('meduza', _('Meduza'),
			_('Configure the Rust Meduza controller. Save & Apply commits /etc/config/meduza; secrets are stored in that root-readable UCI file.'));
		var s = m.section(form.NamedSection, 'main', 'meduza', _('Controller settings'));
		var o;

		s.addremove = false;

		o = s.option(form.Flag, 'enable', _('Enable controller'));
		o.default = o.disabled;
		o.rmempty = false;
		o.description = _('Allow the procd service to run the reconciliation daemon. The meduza init service must also be enabled.');

		o = s.option(form.Value, 'NODE_ID', _('Node ID'));
		o.rmempty = false;
		o.validate = validateNodeId;
		o.placeholder = 'router-01';
		o.description = _('Node name used below /nodes/ and /updated/ in etcd.');

		o = s.option(form.Value, 'ETCD_ENDPOINTS', _('etcd endpoints'));
		o.rmempty = false;
		o.validate = validateEndpoints;
		o.placeholder = 'https://etcd.example.net:2379';
		o.description = _('Comma-separated etcd v3 endpoints. A missing scheme is interpreted as HTTPS.');

		o = s.option(form.Value, 'ETCD_CA', _('CA certificate'));
		o.rmempty = true;
		o.placeholder = '/etc/meduza/pki/ca.crt';
		o.description = _('Absolute path to a PEM CA certificate. HTTPS uses the system CA bundle when this is blank.');

		o = s.option(form.Value, 'ETCD_CERT', _('Client certificate'));
		o.rmempty = true;
		o.validate = pairedValue('ETCD_KEY', _('Client certificate and client key must be configured together.'));
		o.placeholder = '/etc/meduza/pki/client.crt';
		o.description = _('Optional PEM client certificate. Configure it together with the client key.');

		o = s.option(form.Value, 'ETCD_KEY', _('Client private key'));
		o.rmempty = true;
		o.validate = pairedValue('ETCD_CERT', _('Client certificate and client key must be configured together.'));
		o.placeholder = '/etc/meduza/pki/client.key';
		o.description = _('Optional PEM client key. Configure it together with the client certificate.');

		o = s.option(form.Value, 'ETCD_USER', _('etcd username'));
		o.rmempty = true;
		o.validate = pairedValue('ETCD_PASS', _('etcd username and password must be configured together.'));
		o.description = _('Optional username. Configure it together with the password.');

		o = s.option(form.Value, 'ETCD_PASS', _('etcd password'));
		o.password = true;
		o.rmempty = true;
		o.validate = pairedValue('ETCD_USER', _('etcd username and password must be configured together.'));
		o.description = _('Optional password. The input is masked; the value is stored in /etc/config/meduza.');

		o = s.option(form.Value, 'VPN_FIREWALL_ZONE', _('VPN firewall zone'));
		o.rmempty = true;
		o.validate = function(sectionId, value) {
			value = String(value || '').trim();
			if (value.length === 0)
				return true;
			if (value.length > 64 || !/^[A-Za-z0-9_.-]+$/.test(value))
				return _('Use at most 64 ASCII letters, digits, dots, dashes or underscores.');
			if (!firewallAvailable)
				return _('The firewall UCI package is unavailable; leave this option blank.');
			if (!zones[value])
				return _('Select an existing firewall zone.');
			return true;
		};
		o.placeholder = 'lan';
		o.description = firewallAvailable
			? _('Existing firewall zone whose network membership Meduza may update. Leave blank to avoid firewall-zone changes.')
			: _('The firewall UCI package is not installed or cannot be read. Leave this blank unless the target provides a compatible zone configuration.');

		if (firewallAvailable) {
			Object.keys(zones).sort().forEach(function(zone) {
				o.value(zone, zone);
			});
		}

		return m.render();
	}
});
